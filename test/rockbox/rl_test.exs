defmodule Rockbox.EpisodeRegistryTest do
  use ExUnit.Case, async: false

  alias Rockbox.EpisodeRegistry

  setup do
    # Unique ids per test so async-safety isn't an issue with the shared ETS.
    eid = "ep_" <> (:crypto.strong_rand_bytes(6) |> Base.encode16(case: :lower))
    {:ok, episode_id: eid}
  end

  test "lookup misses for unknown episodes" do
    assert EpisodeRegistry.lookup("never_registered_ep") == :miss
  end

  test "register + lookup round trip", %{episode_id: eid} do
    :ok = EpisodeRegistry.register(eid, "vm_test123", "ws_demo")
    assert {:ok, "vm_test123"} = EpisodeRegistry.lookup(eid)
    EpisodeRegistry.forget(eid)
    assert :miss = EpisodeRegistry.lookup(eid)
  end

  test "forget is idempotent", %{episode_id: eid} do
    EpisodeRegistry.register(eid, "vm_x", "ws")
    EpisodeRegistry.forget(eid)
    EpisodeRegistry.forget(eid)
    assert :miss = EpisodeRegistry.lookup(eid)
  end

  test "re-register overwrites the vm mapping", %{episode_id: eid} do
    EpisodeRegistry.register(eid, "vm_a", "ws")
    EpisodeRegistry.register(eid, "vm_b", "ws")
    assert {:ok, "vm_b"} = EpisodeRegistry.lookup(eid)
  end
end

defmodule Rockbox.WireRLTest do
  use ExUnit.Case, async: true

  alias Rockbox.Wire

  describe "rl command encoding" do
    test "rl_step builds the engine's internally-tagged command" do
      cmd = Wire.rl_step("req_1", "ep_1", <<3>>)

      assert %{
               "cmd" => "rl_step",
               "id" => "req_1",
               "episode_id" => "ep_1",
               "action" => %Msgpax.Bin{data: <<3>>}
             } = cmd
    end

    test "rl_steps carries the full action list as bin frames" do
      cmd = Wire.rl_steps("req_2", "ep_2", [<<0>>, <<1>>, <<2>>])
      assert cmd["cmd"] == "rl_steps"

      assert [%Msgpax.Bin{data: <<0>>}, %Msgpax.Bin{data: <<1>>}, %Msgpax.Bin{data: <<2>>}] =
               cmd["actions"]

      assert cmd["episode_id"] == "ep_2"
    end

    test "rl_steps round-trips through msgpack like the Port does" do
      cmd = Wire.rl_steps("req_3", "ep_3", [<<255, 0>>, <<7>>])
      # encode_command is the raw payload; the Port's {:packet, 4} framing adds
      # the length prefix transparently, so unpacking the payload directly is
      # exactly what the engine's FrameReader sees after de-framing. Msgpax.Bin
      # packs to msgpack bin and unpacks back to plain binaries.
      {:ok, decoded} =
        cmd |> Wire.encode_command() |> IO.iodata_to_binary() |> Msgpax.unpack()

      assert decoded["cmd"] == "rl_steps"
      assert decoded["actions"] == [<<255, 0>>, <<7>>]
    end

    test "decoded rl_step responses classify correctly" do
      assert Wire.classify(%{"type" => "rl_steps"}) == :rl_steps
      assert Wire.classify(%{"type" => "rl_step"}) == :rl_step
    end
  end
end
