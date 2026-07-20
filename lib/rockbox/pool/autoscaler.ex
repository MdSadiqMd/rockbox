defmodule Rockbox.Pool.Autoscaler do
  @moduledoc """
  EWMA-based scaling decisions

  - Traffic EWMA  α = 0.3 (scale-up responsiveness)
  - Idle EWMA     α = 0.1 (scale-down conservatism)
  - Scale-up cooldown   30s
  - Scale-down cooldown 5m
  - Max scale-down per cycle 25% of pool
  - Hysteresis: scale-up at util > 80%, scale-down at util < 20%

  Runs on a 2-second tick. Reads pool state from
  [`Rockbox.Pool.Manager.snapshot/0`]
  """

  use GenServer
  require Logger

  @alpha_up 0.3
  @alpha_down 0.1
  @scale_up_cooldown_ms 30_000
  @scale_down_cooldown_ms 300_000
  @util_high 0.80
  @util_low 0.20
  @tick_ms 2_000

  defmodule State do
    @moduledoc false
    defstruct ewma: %{}, last_up: %{}, last_down: %{}
  end

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @impl true
  def init(_) do
    Process.send_after(self(), :tick, @tick_ms)
    {:ok, %State{}}
  end

  @impl true
  def handle_info(:tick, state) do
    Process.send_after(self(), :tick, @tick_ms)
    snapshot = Rockbox.Pool.Manager.snapshot()
    {:noreply, evaluate(snapshot, state)}
  end

  defp evaluate(snapshot, state) do
    groups = group_by_key(snapshot)

    Enum.reduce(groups, state, fn {key, entries}, acc ->
      busy = Enum.count(entries, fn {_, %{state: s}} -> s == :busy end)
      total = length(entries)
      util = if total == 0, do: 0.0, else: busy / total

      prev_up = Map.get(acc.ewma, {key, :up}, util)
      prev_down = Map.get(acc.ewma, {key, :down}, util)
      ewma_up = @alpha_up * util + (1 - @alpha_up) * prev_up
      ewma_down = @alpha_down * util + (1 - @alpha_down) * prev_down

      acc
      |> put_in([Access.key!(:ewma), {key, :up}], ewma_up)
      |> put_in([Access.key!(:ewma), {key, :down}], ewma_down)
      |> maybe_scale(key, ewma_up, ewma_down, total)
    end)
  end

  defp group_by_key(snapshot) do
    Enum.group_by(snapshot, fn {_vm_id, %{key: k}} -> k end)
  end

  defp maybe_scale(state, key, ewma_up, ewma_down, total) do
    now = System.system_time(:millisecond)

    cond do
      ewma_up > @util_high and not in_cooldown?(state.last_up[key], now, @scale_up_cooldown_ms) ->
        Logger.info(
          "autoscaler scale_up key=#{inspect(key)} util_ewma=#{:erlang.float_to_binary(ewma_up, decimals: 2)} total=#{total}"
        )

        # Defer actual spawn to demand-driven path; here we just record cooldown.
        put_in(state, [Access.key!(:last_up), key], now)

      ewma_down < @util_low and total > 1 and
          not in_cooldown?(state.last_down[key], now, @scale_down_cooldown_ms) ->
        retire = max(1, div(total, 4))
        Logger.info("autoscaler scale_down key=#{inspect(key)} retire=#{retire}")
        Rockbox.Pool.Manager.retire(key, retire)
        put_in(state, [Access.key!(:last_down), key], now)

      true ->
        state
    end
  end

  defp in_cooldown?(nil, _now, _ms), do: false
  defp in_cooldown?(ts, now, ms), do: now - ts < ms
end
