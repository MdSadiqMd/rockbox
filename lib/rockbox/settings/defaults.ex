defmodule Rockbox.Settings.Defaults do
  @moduledoc """
  Layered defaults: engine builtin → runtime baseline → workspace overrides → request

  Each layer is a sparse map; later layers override earlier values via
  `deep_merge/2`. Output is the merged request — *not yet clamped* against
  the tier ceiling (that happens in [`Rockbox.Settings.Tiers`])
  """

  alias Rockbox.RuntimeCatalog

  @builtin Application.compile_env(:rockbox, :settings_defaults, %{})

  @doc "Merge layers in the canonical order."
  def merge(request, runtime_name, workspace_defaults \\ %{}, mode \\ :exec) do
    runtime_layer = runtime_defaults(runtime_name, mode)
    mode_layer = mode_defaults(mode)

    @builtin
    |> deep_merge(mode_layer)
    |> deep_merge(runtime_layer)
    |> deep_merge(workspace_defaults)
    |> deep_merge(request)
  end

  def mode_defaults(:exec),
    do: %{
      limits: %{wall_ms: 5_000, memory_mb: 512, output_bytes: 2 * 1024 * 1024},
      lifecycle: %{idle_ttl_s: 0, auto_destroy: true}
    }

  def mode_defaults(:session),
    do: %{
      limits: %{wall_ms: 300_000, memory_mb: 1024, output_bytes: 8 * 1024 * 1024},
      lifecycle: %{idle_ttl_s: 1800, auto_destroy: false},
      capabilities: ["persistent_session"]
    }

  def mode_defaults(:rl_step),
    do: %{
      limits: %{step_ms: 30_000, memory_mb: 4096, output_bytes: 1 * 1024 * 1024},
      capabilities: ["concurrency"]
    }

  def mode_defaults(:rl_episode),
    do: %{
      limits: %{episode_ms: 86_400_000, memory_mb: 16_384, output_bytes: 64 * 1024 * 1024},
      lifecycle: %{idle_ttl_s: 3600, auto_destroy: false},
      capabilities: ["concurrency", "persistent_session"]
    }

  def mode_defaults(_), do: %{}

  defp runtime_defaults(nil, _mode), do: %{}

  defp runtime_defaults(name, _mode) do
    case RuntimeCatalog.lookup(name) do
      nil -> %{}
      runtime -> %{capabilities: runtime.baseline_caps}
    end
  end

  @doc "Recursive map merge (rightmost map wins). Lists are replaced."
  def deep_merge(a, b) when is_map(a) and is_map(b) do
    Map.merge(a, b, fn _k, v1, v2 -> deep_merge(v1, v2) end)
  end

  def deep_merge(_a, b), do: b
end
