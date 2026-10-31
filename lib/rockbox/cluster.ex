defmodule Rockbox.Cluster do
  @moduledoc """
  Multi-node placement for episodes.

  Nodes form an Erlang cluster (`ROCKBOX_CLUSTER_NODES`, comma-separated
  node names; this node reconnects every 2 s) and each publishes its HTTP
  base URL (`ROCKBOX_NODE_URL`). Placement is stateless: an episode id
  minted here is `ep_<tag>_<rand>` where `tag` identifies the node that
  owns it, so any node can route a request for that episode without a
  lookup table. `RockboxWeb.Plugs.ClusterRoute` forwards creates to the
  least-loaded node and episode requests to their owner.

  Single-node deployments (no `ROCKBOX_CLUSTER_NODES`) see no change: ids
  still carry this node's tag, `owner/1` is always local, and the plug is
  a pass-through.
  """

  use GenServer
  require Logger

  @tick_ms 2_000
  @tag_len 6

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc "Base URL of this node for forwarded requests (nil = unconfigured)."
  def local_url, do: Application.get_env(:rockbox, :node_url)

  @doc "Stable short tag for a node name, embedded in episode ids."
  def tag(node \\ node()) do
    :crypto.hash(:sha256, Atom.to_string(node))
    |> Base.encode16(case: :lower)
    |> binary_part(0, @tag_len)
  end

  @doc "Mint an episode id owned by `node`."
  def mint_episode_id(node \\ node()) do
    "ep_" <> tag(node) <> "_" <> Base.url_encode64(:crypto.strong_rand_bytes(9), padding: false)
  end

  @doc """
  Node owning `episode_id`: `:local`, `{:remote, node, url}`, or `:unknown`
  (legacy id or a tag no connected node claims — treat as local).
  """
  def owner("ep_" <> <<tag::binary-size(@tag_len), "_", _::binary>>) do
    cond do
      tag == tag() ->
        :local

      true ->
        case Enum.find(Node.list(), &(tag(&1) == tag)) do
          nil -> :unknown
          node -> remote(node)
        end
    end
  end

  def owner(_), do: :unknown

  @doc """
  Pick the node for a new episode: fewest busy RL engines, local on ties.
  Returns `:local` or `{:remote, node, url}`.
  """
  def place do
    case Node.list() do
      [] ->
        :local

      peers ->
        loads =
          [{node(), local_load()} | Enum.map(peers, &{&1, remote_load(&1)})]
          |> Enum.reject(fn {_, l} -> is_nil(l) end)

        {best, _} = Enum.min_by(loads, fn {n, l} -> {l.busy, n != node()} end)
        if best == node(), do: :local, else: remote(best)
    end
  end

  @doc "This node's load as seen by peers."
  def local_load do
    busy =
      Rockbox.Pool.Manager.snapshot()
      |> Enum.count(fn {_vm, %{key: key, state: s}} ->
        elem(key, 3) in [:rl_step, :rl_episode] and s == :busy
      end)

    %{busy: busy, url: local_url()}
  end

  defp remote_load(node) do
    try do
      :erpc.call(node, __MODULE__, :local_load, [], 1_000)
    catch
      _, _ -> nil
    end
  end

  defp remote(node) do
    case remote_load(node) do
      %{url: url} when is_binary(url) -> {:remote, node, url}
      _ -> :unknown
    end
  end

  @impl true
  def init(_) do
    send(self(), :tick)
    {:ok, %{peers: configured_peers()}}
  end

  @impl true
  def handle_info(:tick, %{peers: peers} = state) do
    for peer <- peers, peer not in Node.list() do
      case Node.connect(peer) do
        true -> Logger.info("cluster: connected #{peer}")
        _ -> :ok
      end
    end

    Process.send_after(self(), :tick, @tick_ms)
    {:noreply, state}
  end

  defp configured_peers do
    (Application.get_env(:rockbox, :cluster_nodes) || "")
    |> String.split(",", trim: true)
    |> Enum.map(&String.trim/1)
    |> Enum.reject(&(&1 == ""))
    |> Enum.map(&String.to_atom/1)
  end
end
