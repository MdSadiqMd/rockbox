defmodule Rockbox.PoolTakeIdleTest do
  use ExUnit.Case, async: false

  alias Rockbox.Pool.Manager
  alias Rockbox.Settings.Effective

  setup do
    ws = "ws_pooltest_" <> Base.encode16(:crypto.strong_rand_bytes(4), case: :lower)
    key = {ws, :python, "py", :exec, nil}
    on_exit(fn -> drain(key) end)
    {:ok, ws: ws, key: key}
  end

  test "empty bucket returns :empty", %{key: key} do
    assert :empty = Manager.take_idle({"nope", :python, "py", :exec, nil}, 0)
    assert :empty = Manager.take_idle(key, 0)
  end

  test "inserted idle VM is taken exactly once", %{key: key} do
    insert_idle(key, "vm_take_once")
    assert {:ok, "vm_take_once", _ts} = Manager.take_idle(key, 0)
    assert :empty = Manager.take_idle(key, 0)
  end

  test "stale VMs are skipped, not terminal", %{key: key} do
    old = System.system_time(:millisecond) - 10_000
    insert_idle(key, "vm_stale", old)
    insert_idle(key, "vm_fresh")
    # ttl 1ms: stale head is destroyed async, fresh VM behind it still dealt.
    assert {:ok, "vm_fresh", _} = Manager.take_idle(key, 1)
    assert :empty = Manager.take_idle(key, 1)
  end

  test "concurrent takes never double-deal", %{key: key} do
    Enum.each(1..32, fn i -> insert_idle(key, "vm_race_#{i}") end)

    results =
      List.duplicate(nil, 64)
      |> Task.async_stream(fn _ -> Manager.take_idle(key, 0) end,
        max_concurrency: 64,
        timeout: 15_000
      )
      |> Enum.map(fn {:ok, r} -> r end)

    oks = for {:ok, vm, _} <- results, do: vm
    assert length(oks) == 32
    assert Enum.uniq(oks) == oks
    assert Enum.count(results, &(&1 == :empty)) == 32
  end

  test "release inserts index, take removes it", %{ws: ws} do
    # Release decrements an existing quota row (production always pairs it
    # with a prior reserve); without this the cast crashes on update_counter.
    assert :ok = Rockbox.QuotaTracker.reserve(ws)
    skey = {ws, :python, "py", :session, nil}
    eff = eff(ws, :session, %{"idle_ttl_s" => 60})
    vm = "vm_rel_" <> Base.encode16(:crypto.strong_rand_bytes(4), case: :lower)
    GenServer.cast(Manager, {:release, vm, eff})
    wait_until(fn -> :ets.lookup(:rockbox_pool_idx, skey) != [] end)
    assert {:ok, ^vm, _} = Manager.take_idle(skey, 0)
    assert :ets.lookup(:rockbox_pool_idx, skey) == []
  end

  test "retire removes main row and index entry", %{key: key} do
    insert_idle(key, "vm_retire")
    assert [{_, _, _}] = :ets.lookup(:rockbox_pool_idx, key)
    GenServer.cast(Manager, {:retire, key, 1})
    wait_until(fn -> :ets.lookup(:rockbox_pool, "vm_retire") == [] end)
    assert :ets.lookup(:rockbox_pool_idx, key) == []
  end

  defp insert_idle(key, vm, ts \\ nil) do
    ts = ts || System.system_time(:millisecond)
    :ets.insert(:rockbox_pool, {vm, %{key: key, state: :idle, ts: ts}})
    :ets.insert(:rockbox_pool_idx, {key, vm, ts})
  end

  defp drain(key) do
    case Manager.take_idle(key, 0) do
      {:ok, vm, _} ->
        :ets.delete(:rockbox_pool, vm)
        drain(key)

      :empty ->
        :ets.match_delete(:rockbox_pool_idx, {key, :_, :_})
        :ok
    end
  end

  defp eff(ws, mode, lifecycle) do
    %Effective{
      request_id: "req_test",
      workspace_id: ws,
      tier: :pro,
      language: :python,
      runtime: "py",
      files: [],
      entrypoint: "main.py",
      mode: mode,
      limits: %{},
      lifecycle: lifecycle,
      capabilities: [],
      network: %{},
      filesystem: %{},
      env: %{},
      resolved_secrets: %{},
      output: %{},
      observability: %{},
      gpu: %{},
      determinism: %{},
      cost: %{},
      labels: %{},
      session_id: nil,
      stdin: nil,
      clamped: []
    }
  end

  defp wait_until(fun, tries \\ 200) do
    cond do
      fun.() -> :ok
      tries <= 0 -> flunk("condition not met within deadline")
      true -> Process.sleep(5) && wait_until(fun, tries - 1)
    end
  end
end
