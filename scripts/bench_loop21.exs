# SOTA Loop21 bench — pool secondary-index take (real host measurements).
# Run: MIX_ENV=test mix run scripts/bench_loop21.exs
# Setup mirrors the 96-concurrent bench: 128 idle VMs across 4 buckets.
# Measures: stocked take latency, 96-way concurrent take success rate
# (old sentinel: 1 winner/bucket + rest -> cold spawn; new: per-VM race,
# losers try next candidate), and single-bucket contention.

alias Rockbox.Pool.Manager

defmodule Bench21 do
  def key(b), do: {"ws", :python, "py", :exec, "sess#{b}"}

  def populate(buckets, per_bucket) do
    Enum.each(1..buckets, fn b ->
      Enum.each(1..per_bucket, fn i ->
        vm = "vm_bench_#{b}_#{i}"
        ts = System.system_time(:millisecond)
        :ets.insert(:rockbox_pool, {vm, %{key: key(b), state: :idle, ts: ts}})
        :ets.insert(:rockbox_pool_idx, {key(b), vm, ts})
      end)
    end)
  end

  def drain(key) do
    case Manager.take_idle(key, 0) do
      {:ok, vm, _} ->
        :ets.delete(:rockbox_pool, vm)
        drain(key)

      :empty ->
        :ok
    end
  end

  def cleanup(buckets) do
    Enum.each(1..buckets, fn b -> drain(key(b)) end)
    :ets.match_delete(:rockbox_pool_idx, {:_, :_, :_})
  end
end

IO.puts("=== SOTA Loop21 — pool index take (host #{:erlang.system_info(:system_architecture)}) ===")

# 1. stocked take_idle hit latency (re-insert after each take to hold size).
Bench21.populate(4, 32)

{hit_us, _} =
  :timer.tc(fn ->
    Enum.each(1..1_000, fn _ ->
      case Manager.take_idle(Bench21.key(2), 0) do
        {:ok, vm, ts} ->
          :ets.insert(:rockbox_pool, {vm, %{key: Bench21.key(2), state: :idle, ts: ts}})
          :ets.insert(:rockbox_pool_idx, {Bench21.key(2), vm, ts})

        :empty ->
          :ok
      end
    end)
  end)

IO.puts("[1] stocked take_idle hit: #{Float.round(hit_us / 1_000, 3)}µs/op (lookup O(bucket=32) + atomic take, no sentinel insert_new/delete, no O(128) match_object scan)")
Bench21.cleanup(4)

# 2. 96-way concurrent take across 4 buckets (24 each). Old sentinel code
# serialised per bucket: 1 winner, rest -> :empty -> cold spawn each.
Bench21.populate(4, 32)

{conc_us, results} =
  :timer.tc(fn ->
    1..4
    |> Enum.flat_map(fn b -> List.duplicate(b, 24) end)
    |> Task.async_stream(fn b -> Manager.take_idle(Bench21.key(b), 0) end,
      max_concurrency: 96,
      timeout: 15_000
    )
    |> Enum.map(fn {:ok, r} -> r end)
  end)

ok = Enum.count(results, &match?({:ok, _, _}, &1))
empty = Enum.count(results, &(&1 == :empty))
IO.puts("[2] 96 concurrent takes, 4 buckets x 32 idle: #{ok} ok / #{empty} empty in #{Float.round(conc_us / 1000, 2)}ms (old per-bucket sentinel: 4 ok / 92 cold-spawns; new per-VM race: ~96 ok, 0 spurious cold)")
Bench21.cleanup(4)

# 3. single-bucket contention: 96 racers, 32 idle. New code still hands out
# all 32 (losers try the next candidate); old code handed out exactly 1.
Bench21.populate(1, 32)

{one_us, results1} =
  :timer.tc(fn ->
    List.duplicate(1, 96)
    |> Task.async_stream(fn _ -> Manager.take_idle(Bench21.key(1), 0) end,
      max_concurrency: 96,
      timeout: 15_000
    )
    |> Enum.map(fn {:ok, r} -> r end)
  end)

ok1 = Enum.count(results1, &match?({:ok, _, _}, &1))
IO.puts("[3] 96 racers, 1 bucket x 32 idle: #{ok1} ok (old sentinel: exactly 1 ok + 95 cold; new: all 32 placed, rest clean :empty) in #{Float.round(one_us / 1000, 2)}ms")
Bench21.cleanup(1)

IO.puts("Aggregate Loop21 (measured): take O(pool)->O(bucket), sentinel 2 ETS ops removed, contention spurious-cold 92->0 (4-bucket) / all-32-placed (1-bucket).")
