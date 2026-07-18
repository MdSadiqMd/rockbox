defmodule Rockbox.Settings.Tiers do
  @moduledoc """
  Per-tier ceilings. Configured in `config/config.exs` and
  override-able per workspace via Postgres in the future

  Returns the clamped settings plus a list of `{path, requested, applied,
  reason}` entries reported back to the caller as `clamped[]`
  """

  @tiers Application.compile_env(:rockbox, :tiers, [])

  @doc "Returns the ceiling map for a tier (e.g. `:free`, `:pro`, `:enterprise`)."
  def ceilings(tier) when is_atom(tier) do
    Keyword.fetch!(@tiers, tier)
  end

  @doc """
  Clamp `settings` against the tier ceiling. Returns
  `{clamped_settings, clamp_log}` where `clamp_log` is the list described
  above.
  """
  def clamp(settings, tier) do
    c = ceilings(tier)

    {settings, []}
    |> clamp_at([:limits, :wall_ms], c.wall_ms_max, "tier_ceiling:#{tier}")
    |> clamp_at([:limits, :memory_mb], c.memory_mb_max, "tier_ceiling:#{tier}")
    |> clamp_at([:limits, :cpu_cores], c.cpu_cores_max, "tier_ceiling:#{tier}")
    |> clamp_at([:gpu, :count], c.gpu_count_max, "tier_ceiling:#{tier}")
    |> clamp_network_tier(c.network_tier_max, tier)
    |> filter_capabilities(c.capabilities_allowed, tier)
  end

  defp clamp_at({settings, log}, path, max, reason) do
    case get_in(settings, path) do
      nil ->
        {settings, log}

      v when is_number(v) and v > max ->
        {put_in(settings, path, max),
         [%{path: path_string(path), requested: v, applied: max, reason: reason} | log]}

      _ ->
        {settings, log}
    end
  end

  defp clamp_network_tier({settings, log}, max_tier, tier) do
    requested = get_in(settings, [:network, :tier]) || "none"

    if tier_rank(requested) > tier_rank(max_tier) do
      {put_in(settings, [:network, :tier], max_tier),
       [
         %{
           path: "network.tier",
           requested: requested,
           applied: max_tier,
           reason: "tier_ceiling:#{tier}"
         }
         | log
       ]}
    else
      {settings, log}
    end
  end

  defp filter_capabilities({settings, log}, allowed, tier) do
    caps = (settings[:capabilities] || []) |> Enum.map(&to_string/1)
    {kept, dropped} = Enum.split_with(caps, &(&1 in allowed))

    new_log =
      Enum.reduce(dropped, log, fn cap, acc ->
        [
          %{
            path: "capabilities[#{cap}]",
            requested: cap,
            applied: nil,
            reason: "tier_ceiling:#{tier}"
          }
          | acc
        ]
      end)

    {Map.put(settings, :capabilities, kept), new_log}
  end

  defp path_string(path), do: Enum.join(path, ".")

  defp tier_rank("none"), do: 0
  defp tier_rank("loopback"), do: 1
  defp tier_rank("egress-allowlist"), do: 2
  defp tier_rank("egress-open"), do: 3
  defp tier_rank(_), do: 0
end
