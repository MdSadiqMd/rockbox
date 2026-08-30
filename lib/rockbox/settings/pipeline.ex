defmodule Rockbox.Settings.Pipeline do
  @moduledoc """
  Implements the 14-step settings pipeline. Returns a frozen [`Rockbox.Settings.Effective`] struct on success

  The pipeline is a pure function from request payload + caller-context
  (workspace + tier) to `{:ok, effective}` or `{:error, reason}`. Side-effecting
  steps (audit log writes, quota reservation) are explicit arguments — pass in
  `:no_op` to skip in tests
  """

  alias Rockbox.Environments
  alias Rockbox.Settings.{CrossField, Defaults, Effective, Tiers, Validator}
  alias Rockbox.SecretsBroker

  @doc """
  Run the pipeline.

  Options:
    * `:workspace_defaults` — sparse map merged after engine + runtime defaults
    * `:reserve_quota` — `fn workspace_id, settings -> :ok | {:error, reason}` end`
    * `:audit_sink` — `fn settings -> :ok end` (called on success)

  Telemetry: emits `[:rockbox, :settings, :pipeline]` with duration + stage.
  """
  def run(payload, ctx, opts \\ []) do
    started_at = System.monotonic_time()

    result =
      with {:ok, validated} <- Validator.validate(payload),
           {:ok, request_id} <- ensure_request_id(validated, ctx),
           {:ok, with_runtime} <- ensure_runtime(validated),
           :ok <- Environments.authorize_runtime(ctx.workspace_id, with_runtime[:runtime]),
           merged <-
             Defaults.merge(
               with_runtime,
               with_runtime[:runtime],
               opts[:workspace_defaults] || %{},
               with_runtime[:mode]
             ),
           {clamped, clamp_log} <- Tiers.clamp(merged, ctx.tier),
           {:ok, validated_cross} <- CrossField.apply(clamped),
           {:ok, resolved} <- resolve_secrets(validated_cross),
           :ok <- maybe_reserve(opts[:reserve_quota], ctx.workspace_id, resolved),
           effective <- freeze(resolved, ctx, request_id, clamp_log) do
        maybe_audit(opts[:audit_sink], effective)
        {:ok, effective}
      end

    duration = System.monotonic_time() - started_at

    :telemetry.execute(
      [:rockbox, :settings, :pipeline],
      %{duration: duration},
      telemetry_meta(result)
    )

    result
  end

  defp ensure_request_id(%{request_id: id}, _) when is_binary(id) and byte_size(id) > 0,
    do: {:ok, id}

  defp ensure_request_id(_s, _ctx) do
    id = "req_" <> (:crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower))
    {:ok, id}
  end

  defp ensure_runtime(%{runtime: r} = s) when is_binary(r), do: {:ok, s}

  defp ensure_runtime(%{language: lang} = s) when is_atom(lang) do
    case Rockbox.RuntimeCatalog.default_for(lang) do
      nil -> {:error, [%{path: "language", reason: "no default runtime for #{lang}"}]}
      entry -> {:ok, Map.put(s, :runtime, entry.name)}
    end
  end

  defp ensure_runtime(s), do: {:ok, s}

  defp resolve_secrets(%{secrets: list} = s) when is_list(list) do
    case SecretsBroker.resolve(list) do
      {:ok, resolved} -> {:ok, Map.put(s, :resolved_secrets, resolved)}
      {:error, reason} -> {:error, [%{path: "secrets", reason: inspect(reason)}]}
    end
  end

  defp resolve_secrets(s), do: {:ok, Map.put(s, :resolved_secrets, %{})}

  defp maybe_reserve(nil, _wid, _s), do: :ok
  defp maybe_reserve(fun, wid, s) when is_function(fun, 2), do: fun.(wid, s)

  defp maybe_audit(nil, _eff), do: :ok
  defp maybe_audit(fun, eff) when is_function(fun, 1), do: fun.(eff)

  defp freeze(s, ctx, request_id, clamp_log) do
    %Effective{
      request_id: request_id,
      workspace_id: ctx.workspace_id,
      tier: ctx.tier,
      language: s[:language],
      runtime: s[:runtime],
      files: s[:files] || [],
      entrypoint: s[:entrypoint] || default_entrypoint(s),
      mode: s[:mode] || :exec,
      session_id: s[:session_id],
      limits: stringify_keys(s[:limits] || %{}),
      lifecycle: stringify_keys(s[:lifecycle] || %{}),
      capabilities: Enum.map(s[:capabilities] || [], &to_string/1),
      network: stringify_keys(s[:network] || %{}),
      filesystem: stringify_keys(s[:filesystem] || %{}),
      env: stringify_kv(s[:env] || %{}),
      resolved_secrets: stringify_kv(s[:resolved_secrets] || %{}),
      stdin: s[:stdin],
      determinism: stringify_keys(s[:determinism] || %{}),
      gpu: stringify_keys(s[:gpu] || %{}),
      output: stringify_keys(s[:output] || %{}),
      observability: stringify_keys(s[:observability] || %{}),
      cost: stringify_keys(s[:cost] || %{}),
      labels: stringify_kv(s[:labels] || %{}),
      clamped: Enum.reverse(clamp_log),
      strict: s[:strict] || false
    }
  end

  defp default_entrypoint(%{language: :python}), do: "main.py"
  defp default_entrypoint(%{language: :typescript}), do: "index.ts"
  defp default_entrypoint(%{language: :go}), do: "main.go"
  defp default_entrypoint(%{language: :rust}), do: "main.rs"
  defp default_entrypoint(%{language: :cpp}), do: "main.cpp"
  defp default_entrypoint(_), do: "main"

  defp stringify_keys(map) when is_map(map),
    do: Map.new(map, fn {k, v} -> {to_string(k), v} end)

  defp stringify_kv(map) when is_map(map),
    do: Map.new(map, fn {k, v} -> {to_string(k), to_string(v)} end)

  defp telemetry_meta({:ok, %Effective{mode: mode, language: lang}}),
    do: %{result: :ok, mode: mode, language: lang}

  defp telemetry_meta({:error, _}), do: %{result: :error}
end
