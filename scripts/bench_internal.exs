# In-container end-to-end benchmark. Run inside the app service (separate
# BEAM, app not started) so HTTP keep-alive works with zero client-side
# process-spawn overhead:
#
#   docker compose exec app elixir -S mix run --no-start scripts/bench_internal.exs
#
# Env:
#   BENCH_MODE  exec | session | rl          (default exec)
#   BENCH_LANG  python | typescript | go | rust | cpp
#   BENCH_ITER  measured iterations          (default 30)
#   BENCH_WARM  warmup iterations            (default 5)
#   BENCH_CONC  concurrency                  (default 1)
#   BENCH_CODE  program source (exec mode)
#   BENCH_URL   (default http://localhost:4000)
#   BENCH_TOKEN (default token-ws_pro_demo-pro)

defmodule Bench do
  @moduledoc false
  import Bitwise

  @headers [
    {"authorization", "Bearer #{System.get_env("BENCH_TOKEN", "token-ws_pro_demo-pro")}"},
    {"content-type", "application/json"}
  ]

  @url System.get_env("BENCH_URL", "http://localhost:4000")

  def lang, do: System.get_env("BENCH_LANG", "python")
  def iter, do: String.to_integer(System.get_env("BENCH_ITER", "30"))
  def warm, do: String.to_integer(System.get_env("BENCH_WARM", "5"))
  def conc, do: String.to_integer(System.get_env("BENCH_CONC", "1"))

  def entrypoint(lang), do: "main.#{ext(lang)}"

  defp ext(:typescript), do: "ts"
  defp ext(lang), do: to_string(lang)

  def code(lang) do
    System.get_env("BENCH_CODE") ||
      case lang do
        "python" -> "print(2+2)"
        "typescript" -> "console.log(2+2)"
        "go" -> "package main\nfunc main(){println(2+2)}"
        "rust" -> "fn main(){println!(\"{}\", 2+2)}"
        "cpp" -> "#include <cstdio>\nint main(){std::printf(\"4\\n\");}"
      end
  end

  def exec_payload(lang) do
    case System.get_env("BENCH_RUNTIME") do
      rt when is_binary(rt) and rt != "" ->
        settings = base_payload(lang)
        %{settings: Map.put(settings, :runtime, rt)}

      _ ->
        %{settings: base_payload(lang)}
    end
  end

  defp base_payload(lang) do
    %{
      language: lang,
      entrypoint: entrypoint(lang),
      files: [%{path: entrypoint(lang), content: code(lang)}],
      limits: %{wall_ms: 5000}
    }
  end

  def post(path, body) do
    t0 = System.monotonic_time(:millisecond)
    resp = Req.post!(path, json: body, base_url: @url, headers: @headers)
    {System.monotonic_time(:millisecond) - t0, resp}
  end

  def bench(label, fun) do
    if warm() > 0 do
      for _ <- 1..warm() do
        fun.()
      end
    end

    times =
      for _ <- 1..iter() do
        fun.()
      end
      |> Enum.sort()

    report(label, times)
  end

  def report(label, times) do
    times = Enum.sort(times)
    n = length(times)
    p = fn q -> Enum.at(times, min(n - 1, trunc(n * q))) end
    avg = Enum.sum(times) / n
    IO.puts("#{label}")

    IO.puts(
      "  n=#{n} min=#{fmt(Enum.min(times))} p50=#{fmt(p.(0.50))} p95=#{fmt(p.(0.95))} p90=#{fmt(p.(0.90))} p99=#{fmt(p.(0.99))} max=#{fmt(Enum.max(times))} avg=#{fmt(avg)} ms"
    )

    IO.puts("  throughput=#{Float.round(1000.0 / avg, 1)} req/s (serial)")
  end

  defp fmt(x) when is_float(x), do: :erlang.float_to_binary(x, decimals: 1)
  defp fmt(x), do: to_string(x)

  def run do
    {:ok, _} = Finch.start_link(name: Req.Finch)

    case System.get_env("BENCH_MODE", "exec") do
      "exec" -> bench_exec()
      "session" -> bench_session()
      "rl" -> bench_rl()
    end
  end

  defp bench_exec do
    lang = lang()
    payload = exec_payload(lang)

    IO.puts("=== exec bench lang=#{lang} conc=#{conc()} ===")

    if conc() == 1 do
      if warm() > 0 do
        for _ <- 1..warm() do
          post("/api/execute", payload)
        end
      end

      {http_times, engine_times} =
        for _ <- 1..iter() do
          {ms, %Req.Response{status: 200, body: body}} = post("/api/execute", payload)
          j = if is_binary(body), do: Jason.decode!(body), else: body
          engine_ms = j["exec_time_ms"]
          {ms, engine_ms}
        end
        |> Enum.reduce({[], []}, fn {h, e}, {hs, es} -> {[h | hs], [e | es]} end)

      report("exec #{lang} (http wall)", Enum.reverse(http_times))
      report("exec #{lang} (engine exec_time_ms)", Enum.reverse(engine_times))
    else
      tasks =
        Task.async_stream(
          1..iter(),
          fn i ->
            {ms, _resp} = post("/api/execute", payload)
            {i, ms}
          end,
          max_concurrency: conc(),
          timeout: 60_000
        )

      times =
        Enum.map(tasks, fn
          {:ok, {_i, ms}} -> ms
          {:exit, reason, _i} -> raise inspect(reason)
        end)
        |> Enum.sort()

      n = length(times)
      p = fn q -> Enum.at(times, min(n - 1, trunc(n * q))) end
      avg = Enum.sum(times) / n
      IO.puts("exec #{lang} (conc=#{conc()})")

      IO.puts(
        "  n=#{n} min=#{fmt(Enum.min(times))} p50=#{fmt(p.(0.50))} p90=#{fmt(p.(0.90))} max=#{fmt(Enum.max(times))} avg=#{fmt(avg)} ms"
      )

      IO.puts("  throughput=#{Float.round(n * 1000.0 / Enum.sum(times), 1)} req/s (aggregated)")
    end
  end

  defp bench_session do
    lang = lang()
    IO.puts("=== session bench lang=#{lang} (via WS) ===")

    payload = %{
      settings: %{
        language: lang,
        entrypoint: entrypoint(lang),
        files: [%{path: entrypoint(lang), content: "x = 0"}],
        limits: %{wall_ms: 5000}
      }
    }

    res = Req.post!("/api/sessions", json: payload, base_url: @url, headers: @headers)
    %{status: 200, body: body} = res
    j = if is_binary(body), do: Jason.decode!(body), else: body
    sid = j["session_id"]

    ws = ws_connect()
    ws = ws_join(ws, "session:#{sid}")
    {:ok, agent} = Agent.start_link(fn -> ws end)

    cell_code =
      case lang do
        "typescript" -> "x = (typeof x === 'undefined' ? 0 : x) + 1"
        _ -> "x = (x if 'x' in globals() else 0) + 1"
      end

    bench("session cell #{lang} (ws round-trip)", fn ->
      t0 = System.monotonic_time(:millisecond)

      ws =
        Agent.get_and_update(agent, fn w ->
          {w, ws_send_cell(w, "session:#{sid}", cell_code)}
        end)

      ws = Agent.get_and_update(agent, fn w -> {w, ws_wait_event(w, "session:cell_result")} end)
      _ = ws
      System.monotonic_time(:millisecond) - t0
    end)

    :gen_tcp.close(agent |> Agent.get(& &1.socket))
    Agent.stop(agent)
    Req.delete!("/api/sessions/#{sid}", base_url: @url, headers: @headers)
  end

  defp bench_rl do
    IO.puts("=== rl bench (gridworld) ===")
    src = File.read!("priv/samples/rl/gridworld.py")

    payload = %{
      settings: %{
        language: "python",
        mode: "rl_step",
        entrypoint: "gridworld.py",
        files: [%{path: "gridworld.py", content: src}],
        limits: %{wall_ms: 5000}
      }
    }

    res = Req.post!("/api/rl/episodes", json: payload, base_url: @url, headers: @headers)
    %{status: 200, body: body} = res
    j = if is_binary(body), do: Jason.decode!(body), else: body
    eid = j["episode_id"]
    vm_id = j["vm_id"]

    action = Base.encode64(<<3>>)

    bench("rl step", fn ->
      {ms, %Req.Response{status: 200}} =
        post("/api/rl/episodes/#{eid}/step?vm_id=#{vm_id}", %{"action" => action})

      ms
    end)

    Req.delete!("/api/rl/episodes/#{eid}?vm_id=#{vm_id}", base_url: @url, headers: @headers)
  end

  # ---- minimal Phoenix-channel WebSocket client (JSON v1.0) ----

  defp ws_url do
    %URI{} = uri = URI.parse(@url)
    {uri.host, uri.port || 80}
  end

  defp ws_connect do
    {host, port} = ws_url()

    {:ok, sock} =
      :gen_tcp.connect(String.to_charlist(host), port, [:binary, active: false, packet: 0])

    key = Base.encode64(:crypto.strong_rand_bytes(16))

    token = System.get_env("BENCH_TOKEN", "token-ws_pro_demo-pro")

    req =
      "GET /socket/websocket?token=#{token}&vsn=1.0.0 HTTP/1.1\r\n" <>
        "Host: #{host}:#{port}\r\n" <>
        "Upgrade: websocket\r\n" <>
        "Connection: Upgrade\r\n" <>
        "Sec-WebSocket-Key: #{key}\r\n" <>
        "Sec-WebSocket-Version: 13\r\n" <>
        "Sec-WebSocket-Protocol: phoenix.json\\v1.0\r\n\r\n"

    :ok = :gen_tcp.send(sock, req)

    response =
      :gen_tcp.recv(sock, 0, 5_000)
      |> elem(1)

    case response do
      "HTTP/1.1 101" <> _ -> :ok
      other -> raise "ws handshake failed: #{inspect(String.slice(other, 0, 40))}"
    end

    %{socket: sock, ref: 0, buf: <<>>}
  end

  defp ws_join(ws, topic) do
    {ref, ws} = next_ref(ws)
    msg = %{"topic" => topic, "event" => "phx_join", "payload" => %{}, "ref" => ref}
    ws_send_raw(ws, Jason.encode!(msg))

    ws = ws_wait_event(ws, "phx_reply")

    case ws do
      %{last: %{"payload" => %{"status" => "ok"}}} -> ws
      %{last: other} -> raise "phx_join failed: #{inspect(other)}"
    end
  end

  defp ws_send_cell(ws, topic, code) do
    {ref, ws} = next_ref(ws)

    msg = %{
      "topic" => topic,
      "event" => "exec_cell",
      "payload" => %{"code" => code, "id" => ref},
      "ref" => ref
    }

    ws_send_raw(ws, Jason.encode!(msg))
    ws
  end

  # Waits for the first message with the given event; skips heartbeats and
  # output pushes. Returns ws with `last` set to the matched message.
  defp ws_wait_event(ws, event) do
    {msg, ws} = ws_read_msg(ws)

    case msg["event"] do
      ^event -> Map.put(ws, :last, msg)
      _ -> ws_wait_event(ws, event)
    end
  end

  defp ws_read_msg(ws) do
    {_opcode, payload, ws} = ws_recv_frame(ws)
    {Jason.decode!(payload), ws}
  end

  defp ws_recv_frame(%{socket: sock, buf: buf} = ws) do
    case take_frame(buf) do
      :need_more ->
        {:ok, data} = :gen_tcp.recv(sock, 0, 10_000)
        ws_recv_frame(%{ws | buf: buf <> data})

      {opcode, payload, tail} ->
        {opcode, payload, %{ws | buf: tail}}
    end
  end

  # Returns {:opcode, payload, rest_buf} or :need_more
  defp take_frame(<<b0, b1, rest::binary>>) do
    len0 = b1 &&& 0x7F

    {header_len, payload_len} =
      cond do
        len0 < 126 ->
          {2, len0}

        len0 == 126 and byte_size(rest) >= 2 ->
          {4, :binary.decode_unsigned(:binary.part(rest, 0, 2))}

        len0 == 127 and byte_size(rest) >= 8 ->
          {10, :binary.decode_unsigned(:binary.part(rest, 0, 8))}

        true ->
          {0, 0}
      end

    if header_len == 0 or byte_size(<<b0, b1, rest::binary>>) < header_len + payload_len do
      :need_more
    else
      <<_h::binary-size(header_len), payload::binary-size(payload_len), tail::binary>> =
        <<b0, b1, rest::binary>>

      {b0 &&& 0x0F, payload, tail}
    end
  end

  defp take_frame(_), do: :need_more

  defp ws_send_raw(%{socket: sock}, payload) do
    header =
      case byte_size(payload) do
        n when n < 126 ->
          <<0x81, 0x80 ||| n>>

        n when n < 65_536 ->
          <<0x81, 0x80 ||| 126, n::16>>

        _ ->
          raise "frame too large"
      end

    mask = :crypto.strong_rand_bytes(4)
    mask_l = :binary.bin_to_list(mask)

    masked =
      payload
      |> :binary.bin_to_list()
      |> Enum.with_index()
      |> Enum.map(fn {b, i} -> Bitwise.bxor(b, Enum.at(mask_l, rem(i, 4))) end)
      |> :binary.list_to_bin()

    :ok = :gen_tcp.send(sock, header <> mask <> masked)
  end

  defp next_ref(ws) do
    ref = Integer.to_string(ws.ref + 1)
    {ref, %{ws | ref: ws.ref + 1}}
  end
end

Bench.run()
