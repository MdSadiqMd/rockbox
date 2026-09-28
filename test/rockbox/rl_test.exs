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

defmodule Rockbox.EpisodeForkTest do
  use ExUnit.Case, async: false

  alias Rockbox.{EpisodeStore, Wire}

  setup do
    root = Path.join(System.tmp_dir!(), "rockbox_fork_#{System.unique_integer([:positive])}")
    File.mkdir_p!(root)
    prev = Application.get_env(:rockbox, :episodes_root)
    Application.put_env(:rockbox, :episodes_root, root)

    on_exit(fn ->
      File.rm_rf!(root)

      if prev,
        do: Application.put_env(:rockbox, :episodes_root, prev),
        else: Application.delete_env(:rockbox, :episodes_root)
    end)

    {:ok, root: root}
  end

  test "rl_snapshot wire command is internally tagged like the other rl commands" do
    assert %{"cmd" => "rl_snapshot", "id" => "req_9", "episode_id" => "ep_9"} =
             Wire.rl_snapshot("req_9", "ep_9")
  end

  test "clone_episode copies manifest with child id plus checkpoint", %{root: root} do
    parent = Path.join(root, "ep_parent")
    File.mkdir_p!(parent)

    File.write!(
      Path.join(parent, "manifest.json"),
      Jason.encode!(%{"request_id" => "ep_parent", "workspace_id" => "ws_a", "mode" => "rl_step"})
    )

    File.write!(Path.join(parent, "state.pkl"), <<128, 4, 1, 2, 3>>)
    File.write!(Path.join(parent, "user_file.txt"), "not copied")

    assert :ok = EpisodeStore.clone_episode("ep_parent", "ep_child")

    assert {:ok, %{"request_id" => "ep_child", "workspace_id" => "ws_a", "mode" => "rl_step"}} =
             EpisodeStore.fetch_settings("ep_child")

    assert File.read!(Path.join([root, "ep_child", "state.pkl"])) == <<128, 4, 1, 2, 3>>
    refute File.exists?(Path.join([root, "ep_child", "user_file.txt"]))
    # Parent untouched — forks never move state.
    assert {:ok, %{"request_id" => "ep_parent"}} = EpisodeStore.fetch_settings("ep_parent")
  end

  test "clone_episode without a checkpoint still yields a resumable manifest", %{root: root} do
    parent = Path.join(root, "ep_nostate")
    File.mkdir_p!(parent)

    File.write!(
      Path.join(parent, "manifest.json"),
      Jason.encode!(%{"request_id" => "ep_nostate"})
    )

    assert :ok = EpisodeStore.clone_episode("ep_nostate", "ep_child2")
    refute File.exists?(Path.join([root, "ep_child2", "state.pkl"]))
    assert {:ok, %{"request_id" => "ep_child2"}} = EpisodeStore.fetch_settings("ep_child2")
  end

  test "clone_episode of an unknown parent fails cleanly" do
    assert {:error, :no_manifest} = EpisodeStore.clone_episode("ep_missing", "ep_x")
  end
end
