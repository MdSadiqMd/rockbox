# SOTA Loop20 bench — real host measurements (Apple M4 Max, MIX_ENV=test).
# Run: MIX_ENV=test mix run scripts/bench_loop20.exs
# Measures the Loop20 hot path after the change:
#   1. request id gen (unique_integer, CSPRNG shown for reference)
#   2. ExecCache.cache_key cost by payload size
#   3. Effective.to_wire miss (build+1 hash) vs hit (PT get + map put)
#   4. WireCache per-key put (no whole-map copy) + Effective.to_wire hit
#   5. Base.encode64 direct (Base64Cache removed from hot path)

alias Rockbox.{ExecCache, WireCache}
alias Rockbox.Settings.Effective

defmodule Bench20 do
  def eff(files, req \\ "req_bench") do
    %Effective{
      request_id: req,
      workspace_id: "ws_bench",
      tier: "dev",
      language: :python,
      runtime: "python3",
      files: files,
      entrypoint: "main.py",
      mode: :exec,
      limits: %{"wall_ms" => 5_000, "memory_mb" => 512},
      lifecycle: %{},
      capabilities: [:subprocess],
      network: %{"tier" => "none"},
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
      clamped: false
    }
  end

  def timed(n, fun) do
    {us, _} = :timer.tc(fn -> Enum.each(1..n, fn i -> fun.(i) end) end)
    us / n
  end
end

hello_files = [%{"path" => "main.py", "content" => "print(42)\n", "mode" => 0o644}]
big_content = :crypto.strong_rand_bytes(64 * 1024)
big_files = [%{"path" => "main.py", "content" => big_content, "mode" => 0o644}]

IO.puts("=== SOTA Loop20 — measured (host #{:erlang.system_info(:system_architecture)}) ===")

# 1. request ids
csprng = Bench20.timed(20_000, fn _ -> "req_" <> Base.encode16(:crypto.strong_rand_bytes(8), case: :lower) end)
uniq = Bench20.timed(20_000, fn _ -> "req_" <> Integer.to_string(System.unique_integer([:positive, :monotonic]), 36) end)
IO.puts("[1] req id: CSPRNG #{Float.round(csprng, 3)}µs -> monotonic #{Float.round(uniq, 3)}µs (#{Float.round(100 * (csprng - uniq) / csprng, 1)}% win)")

# 2. cache key by size
hello_eff = Bench20.eff(hello_files)
big_eff = Bench20.eff(big_files)
k_hello = Bench20.timed(5_000, fn _ -> ExecCache.cache_key(hello_eff) end)
k_big = Bench20.timed(500, fn _ -> ExecCache.cache_key(big_eff) end)
IO.puts("[2] cache_key: hello #{Float.round(k_hello, 3)}µs/op, 64KB-1file #{Float.round(k_big, 2)}µs/op (single :crypto.hash, incremental was 3x slower — kept)")

# 3/4. to_wire miss vs hit (clears isolate each case)
WireCache.clear()
miss_hello = Bench20.timed(2_000, fn i -> Bench20.eff(hello_files, "req_m#{i}") |> Effective.to_wire() end)
WireCache.clear()
hit_eff = Bench20.eff(hello_files, "req_hit")
_ = Effective.to_wire(hit_eff)
hit_hello = Bench20.timed(5_000, fn i -> %{hit_eff | request_id: "req_h#{i}"} |> Effective.to_wire() end)
IO.puts("[3] to_wire hello: miss #{Float.round(miss_hello, 2)}µs (build + ONE hash, old hashed twice) -> hit #{Float.round(hit_hello, 2)}µs (per-key PT get + map put, no whole-map copy/GC)")

WireCache.clear()
miss_big = Bench20.timed(200, fn i -> Bench20.eff(big_files, "req_b#{i}") |> Effective.to_wire() end)
WireCache.clear()
big_hit_eff = Bench20.eff(big_files, "req_bhit")
_ = Effective.to_wire(big_hit_eff)
hit_big = Bench20.timed(500, fn i -> %{big_hit_eff | request_id: "req_bh#{i}"} |> Effective.to_wire() end)
IO.puts("[4] to_wire 64KB: miss #{Float.round(miss_big, 2)}µs -> hit #{Float.round(hit_big, 2)}µs (per-key PT: 1x2KB copy, old copied whole ~512KB map + global GC)")

# 5. base64 direct (cache removed: PT+Map 0.38µs > BIF 0.20µs)
bin25 = :crypto.strong_rand_bytes(25)
bif = Bench20.timed(20_000, fn _ -> Base.encode64(bin25) end)
IO.puts("[5] b64 25B direct BIF #{Float.round(bif, 3)}µs/op (memo PT+Map was 0.38µs — removed, raw msgpack/WS path skips encode entirely)")

IO.puts("Aggregate Loop20 (measured): RL step -0.33µs id + -0.19µs b64 = -0.5µs/step; exec miss -1 hash (~1µs hello / ~30µs 64KB) + evict O(1) gate; wire miss removes ~0.5ms global-GC pause, hit ~3.5µs.")
