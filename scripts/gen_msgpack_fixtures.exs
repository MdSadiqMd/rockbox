# Generate msgpack fixtures with the REAL server encoder (Msgpax) for the
# vendored Python decoder. Run: MIX_ENV=test mix run scripts/gen_msgpack_fixtures.exs
# Writes /tmp/mp_fix{N}.bin (packed) + /tmp/mp_fix{N}.json (expected, obs hex).

defmodule Fix do
  def tick(obs, info, meta, reward \\ 1.5) do
    # Same shape as RLController.tick_raw/2 (atom keys, plain binary obs).
    %{
      episode_id: "ep_1",
      request_id: "req_9",
      observation: obs,
      reward: reward,
      done: false,
      terminated: false,
      truncated: false,
      info: info,
      obs_meta: meta
    }
  end

  def write(n, term, expected) do
    packed = term |> Msgpax.pack!() |> IO.iodata_to_binary()
    File.write!("/tmp/mp_fix#{n}.bin", packed)
    File.write!("/tmp/mp_fix#{n}.json", Jason.encode!(expected))
    IO.puts("fixture #{n}: #{byte_size(packed)} bytes packed")
  end
  def exp(obs, info, meta, reward \\ 1.5) do
    %{
      "episode_id" => "ep_1",
      "request_id" => "req_9",
      "observation_hex" => Base.encode16(obs),
      "reward" => reward,
      "done" => false,
      "terminated" => false,
      "truncated" => false,
      "info" => info,
      "obs_meta" => meta
    }
  end
end

# 1. tiny ASCII obs (valid UTF-8 — the ambiguous str-vs-bytes case)
obs1 = String.duplicate("A", 25)
Fix.write(1, Fix.tick(obs1, %{}, nil), Fix.exp(obs1, %{}, nil))

# 2. 64KB random obs + info + obs_meta
obs2 = :crypto.strong_rand_bytes(64 * 1024)
meta2 = %{"dtype" => "uint8", "shape" => "[84,84]"}
Fix.write(2, Fix.tick(obs2, %{"steps" => "3"}, meta2, -0.5), Fix.exp(obs2, %{"steps" => "3"}, meta2, -0.5))

# 3. batch shape (string keys, like send_ticks) with metrics
batch = %{
  "episode_id" => "ep_1",
  "ticks" => [Fix.tick(obs1, %{}, nil), Fix.tick(<<>>, %{"error" => "boom"}, nil, 0.0)],
  "metrics" => %{"steps" => 32, "reward_sum" => 12.5, "elapsed_ms" => 61}
}

exp_batch = %{
  "episode_id" => "ep_1",
  "ticks" => [Fix.exp(obs1, %{}, nil), Fix.exp(<<>>, %{"error" => "boom"}, nil, 0.0)],
  "metrics" => %{"steps" => 32, "reward_sum" => 12.5, "elapsed_ms" => 61}
}

Fix.write(3, batch, exp_batch)
