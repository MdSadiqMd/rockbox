defmodule Rockbox.EffectiveCacheKeyTest do
  use ExUnit.Case, async: false

  alias Rockbox.{ExecCache, WireCache}
  alias Rockbox.Settings.{Effective, Pipeline}

  @ctx %{workspace_id: "ws_keytest", tier: :pro, user_id: "u1"}

  setup do
    WireCache.clear()
    :ok
  end

  test "pipeline freezes a stable content key, independent of request_id" do
    payload = %{
      "language" => "python",
      "files" => [%{"path" => "main.py", "content" => "print(42)\n"}]
    }

    assert {:ok, %Effective{} = a} = Pipeline.run(payload, @ctx)
    assert {:ok, %Effective{} = b} = Pipeline.run(payload, @ctx)
    assert is_binary(a.cache_key)
    assert a.request_id != b.request_id
    assert a.cache_key == b.cache_key
    assert ExecCache.key(a) == ExecCache.cache_key(a)
  end

  test "different content means different key" do
    run = fn content ->
      {:ok, eff} =
        Pipeline.run(
          %{"language" => "python", "files" => [%{"path" => "main.py", "content" => content}]},
          @ctx
        )

      eff
    end

    assert run.("print(1)").cache_key != run.("print(2)").cache_key
  end

  test "non-cacheable requests skip the hash" do
    {:ok, eff} =
      Pipeline.run(
        %{
          "language" => "python",
          "files" => [%{"path" => "main.py", "content" => "print(1)"}],
          "stdin" => %{"text" => "hi"}
        },
        @ctx
      )

    assert eff.cache_key == nil
    refute ExecCache.cacheable?(eff)
  end

  test "to_wire reuses the frozen key: second build is a hit" do
    {:ok, eff} =
      Pipeline.run(
        %{"language" => "python", "files" => [%{"path" => "main.py", "content" => "print(1)"}]},
        @ctx
      )

    wire1 = Effective.to_wire(eff)
    wire2 = Effective.to_wire(eff)
    assert wire1 == wire2
    assert wire1["request_id"] == eff.request_id
  end
end
