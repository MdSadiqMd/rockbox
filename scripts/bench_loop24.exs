# Loop24 probe — where does Elixir batch time go? (host-measured)
# Measures per-32-tick-batch costs the RL hot path pays AFTER the engine
# answers: action base64 decode, tick shaping, JSON vs msgpack response encode.
# Run: MIX_ENV=test mix run scripts/bench_loop24.exs

defmodule Bench24 do
  def tick(obs) do
    %{
      "request_id" => "req_x",
      "observation" => obs,
      "reward" => 1.0,
      "done" => false,
      "terminated" => false,
      "truncated" => false,
      "info" => %{},
      "obs_meta" => nil
    }
  end

  def timed(n, fun) do
    {us, _} = :timer.tc(fn -> Enum.each(1..n, fn _ -> fun.() end) end)
    us / n
  end
end

for {label, obs} <- [
      {"25B", :crypto.strong_rand_bytes(25)},
      {"1KB", :crypto.strong_rand_bytes(1024)},
      {"64KB", :crypto.strong_rand_bytes(64 * 1024)}
    ] do
  ticks = Enum.map(1..32, fn _ -> Bench24.tick(obs) end)
  actions = Enum.map(1..32, fn _ -> Base.encode64(:crypto.strong_rand_bytes(8)) end)

  dec =
    Bench24.timed(500, fn ->
      Enum.map(actions, &Base.decode64!/1)
    end)

  # JSON shape + encode (what the send_ticks JSON path pays)
  shaped =
    Enum.map(ticks, fn m ->
      %{
        episode_id: "ep",
        request_id: m["request_id"],
        observation: Base.encode64(m["observation"]),
        reward: m["reward"],
        done: m["done"],
        terminated: m["terminated"],
        truncated: m["truncated"],
        info: m["info"],
        obs_meta: m["obs_meta"]
      }
    end)

  jenc = Bench24.timed(100, fn -> Jason.encode!(%{episode_id: "ep", ticks: shaped, metrics: %{}}) end)

  # msgpack shape + encode (raw-bytes path)
  raw =
    Enum.map(ticks, fn m ->
      %{
        episode_id: "ep",
        request_id: m["request_id"],
        observation: m["observation"],
        reward: m["reward"],
        done: m["done"],
        terminated: m["terminated"],
        truncated: m["truncated"],
        info: m["info"],
        obs_meta: m["obs_meta"]
      }
    end)

  menc = Bench24.timed(100, fn -> Msgpax.pack!(%{"episode_id" => "ep", "ticks" => raw, "metrics" => %{}}) end)

  IO.puts(
    "#{label} x32/batch: decode #{Float.round(dec, 1)}µs | " <>
      "JSON shape+encode #{Float.round(jenc, 1)}µs | msgpack raw #{Float.round(menc, 1)}µs"
  )
end
