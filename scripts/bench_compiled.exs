# Compiled-language bench for perf profiling: cold (unique programs) then
# warm (identical program). Keep-alive client, no per-request spawn overhead.
#   docker compose exec app elixir -S mix run --no-start scripts/bench_compiled.exs
# Env: BENCH_LANG go|rust|cpp, BENCH_COLD n, BENCH_WARM n

defmodule BenchC do
  @headers [
    {"authorization", "Bearer #{System.get_env("BENCH_TOKEN", "token-ws_pro_demo-pro")}"},
    {"content-type", "application/json"}
  ]
  @url System.get_env("BENCH_URL", "http://localhost:4000")

  def lang, do: System.get_env("BENCH_LANG", "go")
  def cold_n, do: String.to_integer(System.get_env("BENCH_COLD", "6"))
  def warm_n, do: String.to_integer(System.get_env("BENCH_WARM", "30"))

  defp entry(lang) do
    case lang do
      "go" -> "main.go"
      "rust" -> "main.rs"
      "cpp" -> "main.cpp"
    end
  end

  defp content(lang, i) do
    case lang do
      "go" -> "package main\nimport \"fmt\"\nfunc main(){fmt.Println(2+2+#{i})}"
      "rust" -> "fn main(){println!(\"{}\", 2+2+#{i})}"
      "cpp" -> "#include <cstdio>\nint main(){std::printf(\"%d\\n\", 2+2+#{i});}"
    end
  end

  defp payload(lang, content) do
    %{
      settings: %{
        language: lang,
        entrypoint: entry(lang),
        files: [%{path: entry(lang), content: content}],
        limits: %{wall_ms: 5000}
      }
    }
  end

  defp run(body) do
    t0 = System.monotonic_time(:millisecond)
    resp = Req.post!("/api/execute", json: body, base_url: @url, headers: @headers)
    ms = System.monotonic_time(:millisecond) - t0
    j = if is_binary(resp.body), do: Jason.decode!(resp.body), else: resp.body
    {ms, j["exec_time_ms"], j["status"]}
  end

  def run_bench do
    {:ok, _} = Finch.start_link(name: Req.Finch)
    l = lang()

    IO.puts("=== cold #{l} (#{cold_n()}) ===")
    for i <- 1..cold_n() do
      {ms, eng, status} = run(payload(l, content(l, i)))
      IO.puts("#{i}: wall=#{ms}ms engine=#{eng}ms status=#{status}")
    end

    IO.puts("=== warm #{l} (#{warm_n()}) ===")
    {times, engines} =
      Enum.reduce(1..warm_n(), {[], []}, fn _, {ts, es} ->
        {ms, eng, _} = run(payload(l, content(l, 999)))
        {[ms | ts], [eng | es]}
      end)

    t = Enum.sort(times)
    e = Enum.sort(engines)
    p = fn q, list -> Enum.at(list, min(length(list) - 1, trunc(length(list) * q))) end
    avg = fn list -> Enum.sum(list) / length(list) end
    IO.puts(
      "warm wall: min=#{Enum.min(t)} p50=#{p.(0.5, t)} p90=#{p.(0.9, t)} max=#{Enum.max(t)} avg=#{Float.round(avg.(t), 1)} ms"
    )
    IO.puts("warm engine: min=#{Enum.min(e)} p50=#{p.(0.5, e)} max=#{Enum.max(e)} ms")
  end
end

BenchC.run_bench()