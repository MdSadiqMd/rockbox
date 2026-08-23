# Data-channel end-to-end check: subscribes to "vm:<id>" on the live node,
# runs a chatty program on that VM, counts {:stdout, _} broadcasts.
#
#   docker compose exec app elixir --sname probe -S mix run --no-start scripts/check_data_channel.exs

Node.start(:"probe@127.0.0.1")
{:ok, host} = :inet.gethostname()
target = :"rockbox@#{host}"

cookie = File.read!(Path.expand("~/.erlang.cookie")) |> String.trim() |> String.to_atom()
true = Node.set_cookie(cookie)
true = Node.connect(target)

code = """
payload = %{
  "language" => "python",
  "entrypoint" => "main.py",
  "files" => [%{"path" => "main.py", "content" => "for i in range(200): print('line', i)"}],
  "limits" => %{"wall_ms" => 5000}
}

ctx = %{workspace_id: "ws_pro_demo", tier: :pro, user_id: nil}

{:ok, eff} =
  Rockbox.Settings.Pipeline.run(payload, ctx,
    audit_sink: fn _ -> :ok end,
    reserve_quota: fn _, _ -> :ok end
  )

{:ok, vm_id} = Rockbox.Pool.Manager.acquire(eff)
Phoenix.PubSub.subscribe(Rockbox.PubSub, "vm:\#{vm_id}")

{:ok, result} = Rockbox.VM.Server.execute_and_wait(vm_id, eff, 10_000)
Process.sleep(300)

{:messages, msgs} = Process.info(self(), :messages)
stdout_chunks = Enum.count(msgs, &match?({:stdout, _}, &1))
stderr_chunks = Enum.count(msgs, &match?({:stderr, _}, &1))

IO.puts("result_status=\#{result["status"]} bytes=\#{byte_size(result["output"])}")
IO.puts("data_channel_stdout_chunks=\#{stdout_chunks} stderr_chunks=\#{stderr_chunks}")
Rockbox.Pool.Manager.release(vm_id, eff)

if stdout_chunks > 0 do
  IO.puts("DATA_CHANNEL_OK")
else
  IO.puts("DATA_CHANNEL_SILENT")
end
"""

:rpc.block_call(target, Code, :eval_string, [code])
System.halt(0)
