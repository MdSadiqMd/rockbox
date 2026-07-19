defmodule Rockbox.Settings.CrossField do
  @moduledoc """
  Cross-field validation rules

  These are explicit, code-reviewed rules — no implicit coercion. Some rules
  auto-fix (e.g. add an implied capability); the rest return `{:error, _}`

  Output: `{:ok, settings}` or `{:error, [violation]}` where each violation
  is `%{path, requested, reason, suggestion}`
  """

  @type violation :: %{
          path: String.t(),
          requested: any(),
          reason: String.t(),
          suggestion: String.t() | nil
        }

  def apply(settings) do
    {settings, []}
    |> rule_session_id_required()
    |> rule_session_persistent_auto_add()
    |> rule_gpu_count_and_cap()
    |> rule_install_needs_network()
    |> rule_language_runtime_match()
    |> rule_cpu_time_sanity()
    |> rule_determinism_no_network()
    |> finalize()
  end

  defp rule_session_id_required({s, errs}) do
    case {s[:mode], s[:session_id]} do
      {"session", id} when id in [nil, ""] ->
        {s, [violation("session_id", nil, "mode=session requires session_id") | errs]}

      {"exec", id} when not is_nil(id) ->
        {s,
         [
           violation(
             "session_id",
             id,
             "session_id present but mode=exec",
             "Set mode=session or remove session_id."
           )
           | errs
         ]}

      _ ->
        {s, errs}
    end
  end

  defp rule_session_persistent_auto_add({s, errs}) do
    if s[:mode] == "session" and "persistent_session" not in (s[:capabilities] || []) do
      caps = (s[:capabilities] || []) ++ ["persistent_session"]
      {Map.put(s, :capabilities, caps), errs}
    else
      {s, errs}
    end
  end

  defp rule_gpu_count_and_cap({s, errs}) do
    caps = s[:capabilities] || []
    gpu = (s[:gpu] || %{})[:count] || 0

    cond do
      "gpu" in caps and gpu == 0 ->
        {s, [violation("gpu.count", 0, "+gpu capability requires gpu.count > 0") | errs]}

      gpu > 0 and "gpu" not in caps ->
        # Auto-add the capability (matches §21.7 "transparent").
        {Map.put(s, :capabilities, caps ++ ["gpu"]), errs}

      true ->
        {s, errs}
    end
  end

  defp rule_install_needs_network({s, errs}) do
    caps = s[:capabilities] || []
    tier = get_in(s, [:network, :tier]) || "none"

    if "install" in caps and tier == "none" do
      {s,
       [
         violation(
           "capabilities[install]",
           "install",
           "+install requires network>=egress-allowlist",
           "Raise settings.network.tier."
         )
         | errs
       ]}
    else
      {s, errs}
    end
  end

  defp rule_language_runtime_match({s, errs}) do
    lang = s[:language]
    runtime = s[:runtime]

    case Rockbox.RuntimeCatalog.lookup(runtime) do
      nil when is_binary(runtime) ->
        {s, [violation("runtime", runtime, "unknown runtime in catalog") | errs]}

      %Rockbox.RuntimeCatalog.Entry{language: rl} when not is_nil(lang) ->
        if Atom.to_string(rl) == to_string(lang) do
          {s, errs}
        else
          {s,
           [
             violation(
               "runtime",
               runtime,
               "runtime #{runtime} (#{rl}) does not match language=#{lang}"
             )
             | errs
           ]}
        end

      _ ->
        {s, errs}
    end
  end

  defp rule_cpu_time_sanity({s, errs}) do
    limits = s[:limits] || %{}

    case {limits[:cpu_ms], limits[:wall_ms], limits[:cpu_cores]} do
      {cpu_ms, wall_ms, cores}
      when is_number(cpu_ms) and is_number(wall_ms) and is_number(cores) ->
        if cpu_ms > wall_ms * cores * 1.5 do
          {s,
           [
             violation(
               "limits.cpu_ms",
               cpu_ms,
               "cpu_ms (#{cpu_ms}) > wall_ms × cpu_cores × 1.5 — impossible"
             )
             | errs
           ]}
        else
          {s, errs}
        end

      _ ->
        {s, errs}
    end
  end

  defp rule_determinism_no_network({s, errs}) do
    if get_in(s, [:determinism, :deterministic_time]) == true and
         get_in(s, [:network, :tier]) not in [nil, "none", "loopback"] do
      {s,
       [
         violation(
           "determinism.deterministic_time",
           true,
           "deterministic time + network is non-deterministic"
         )
         | errs
       ]}
    else
      {s, errs}
    end
  end

  defp finalize({s, []}), do: {:ok, s}
  defp finalize({_s, errs}), do: {:error, Enum.reverse(errs)}

  defp violation(path, requested, reason, suggestion \\ nil),
    do: %{path: path, requested: requested, reason: reason, suggestion: suggestion}
end
