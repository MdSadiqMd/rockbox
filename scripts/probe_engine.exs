# Probe: talk to the engine binary directly (same Port setup as VM.Server),
# send one Execute, print frames with timing. Diagnoses host exec hangs.
alias Rockbox.Settings.{Effective, Pipeline}

ctx = %{workspace_id: "ws_probe", tier: :pro, user_id: "u1"}

{:ok, eff} =
  Pipeline.run(
    %{"language" => "python", "files" => [%{"path" => "main.py", "content" => "print(42)\n"}]},
    ctx
  )

bin = Application.get_env(:rockbox, :engine)[:binary]
sock_path = "/tmp/rockbox-probe.sock"
File.rm(sock_path)

port =
  Port.open({:spawn_executable, bin}, [
    :binary,
    :exit_status,
    {:packet, 4},
    {:args, ["--data-socket", sock_path, "--log", "info"]}
  ])

t0 = System.monotonic_time(:millisecond)

receive do
  {^port, {:data, bytes}} ->
    dt = System.monotonic_time(:millisecond) - t0

    case Msgpax.unpack(bytes) do
      {:ok, %{"type" => type} = msg} ->
        IO.puts("ready in #{dt}ms type=#{type} keys=#{inspect(Map.keys(msg))}")

      other ->
        IO.puts("ready undecodable in #{dt}ms: #{inspect(other) |> String.slice(0, 200)}")
    end
after
  5_000 -> IO.puts("NO READY in 5000ms")
end

payload = Map.merge(%{"cmd" => "execute"}, Effective.to_wire(eff))
Port.command(port, Msgpax.pack!(payload, iodata: true))
t1 = System.monotonic_time(:millisecond)

receive do
  {^port, {:data, bytes}} ->
    dt = System.monotonic_time(:millisecond) - t1

    case Msgpax.unpack(bytes) do
      {:ok, %{"type" => type} = msg} ->
        small = Map.drop(msg, ["output"])
        IO.puts("response in #{dt}ms type=#{type} #{inspect(small) |> String.slice(0, 400)}")

      other ->
        IO.puts("response undecodable in #{dt}ms: #{inspect(other) |> String.slice(0, 200)}")
    end
after
  8_000 -> IO.puts("NO RESPONSE in 8000ms")
end
