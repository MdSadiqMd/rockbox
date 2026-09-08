defmodule Rockbox.Settings.Effective do
  @moduledoc """
  Immutable, frozen settings produced by [`Rockbox.Settings.Pipeline.run/2`]

  Every value here is post-merge, post-clamp, post-secrets-resolution.
  Downstream code (VM GenServer, AuditLog, CostTracker, engine) reads only
  this struct — no further re-resolution allowed
  """

  @enforce_keys [
    :request_id,
    :workspace_id,
    :tier,
    :language,
    :runtime,
    :files,
    :entrypoint,
    :mode,
    :limits,
    :lifecycle,
    :capabilities,
    :network,
    :filesystem,
    :env,
    :resolved_secrets,
    :output,
    :observability,
    :gpu,
    :determinism,
    :cost,
    :labels,
    :session_id,
    :stdin,
    :clamped
  ]

  defstruct @enforce_keys ++ [strict: false, cache_key: nil]

  @typedoc """
  A frozen Settings struct — never mutated after `Pipeline.run/2` returns.
  Pass it by reference (Elixir's structural sharing keeps this cheap).
  """
  @type t :: %__MODULE__{
          request_id: String.t(),
          workspace_id: String.t(),
          tier: atom(),
          language: atom(),
          runtime: String.t(),
          files: [map()],
          entrypoint: String.t(),
          mode: atom(),
          limits: map(),
          lifecycle: map(),
          capabilities: [atom() | String.t()],
          network: map(),
          filesystem: map(),
          env: %{String.t() => String.t()},
          resolved_secrets: %{String.t() => String.t()},
          output: map(),
          observability: map(),
          gpu: map(),
          determinism: map(),
          cost: map(),
          labels: %{String.t() => String.t()},
          session_id: String.t() | nil,
          clamped: [map()],
          strict: boolean(),
          # SOTA Loop22: frozen content hash (`ExecCache.cache_key/1`),
          # attached by `Pipeline.freeze/4` when cacheable, else nil.
          cache_key: binary() | nil
        }

  @doc """
  Build the msgpack-shaped map the Rust engine expects. Keys are strings,
  enum atoms are stringified, file `content` is sent as raw bytes.
  """
  def to_wire(%__MODULE__{} = s) do
    # SOTA Loop20: one hash per build. Old path hashed twice on miss
    # (`WireCache.get` + `WireCache.put` each hashed). Now the key is
    # computed once and threaded through both.
    key = Rockbox.ExecCache.key(s)

    case Rockbox.WireCache.get_with_key(s, key) do
      {:ok, wire} ->
        wire

      :miss ->
        wire = %{
          "schema" => Rockbox.Wire.schema_version(),
          "request_id" => s.request_id,
          "labels" => s.labels,
          "language" => Atom.to_string(s.language),
          "runtime" => s.runtime,
          "files" => Enum.map(s.files, &file_to_wire/1),
          "entrypoint" => s.entrypoint,
          "mode" => Atom.to_string(s.mode),
          "session_id" => s.session_id,
          "limits" => s.limits,
          "lifecycle" => s.lifecycle,
          "capabilities" => Enum.map(s.capabilities, &wire_cap/1),
          "network" => s.network,
          "filesystem" => s.filesystem,
          "env" => s.env,
          "resolved_secrets" => s.resolved_secrets,
          "stdin" => s.stdin,
          "determinism" => s.determinism,
          "gpu" => s.gpu,
          "output" => s.output,
          "observability" => s.observability,
          "cost" => s.cost
        }

        Rockbox.WireCache.put_with_key(s, wire, key)
        wire
    end
  end

  defp file_to_wire(%{} = f) do
    %{
      "path" => f["path"] || f[:path],
      "content" => f["content"] || f[:content],
      "mode" => f["mode"] || f[:mode] || 0o644
    }
  end

  # Pipeline freezes capabilities as strings ("concurrency"); hand-built
  # structs (benches, tests) use atoms. The engine wants strings either way.
  defp wire_cap(a) when is_atom(a), do: Atom.to_string(a)
  defp wire_cap(s) when is_binary(s), do: s
end
