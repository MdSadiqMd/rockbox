# VM round-trip attribution: connects to the live app node and RPCs the
# acquire -> execute_and_wait -> release loop there (needs the real port +
# engine). Run:
#
#   docker compose exec app elixir --sname probe -S mix run --no-start scripts/profile_vm_roundtrip.exs

Node.start(:"probe@127.0.0.1")
{:ok, host} = :inet.gethostname()
target = :"rockbox@#{host}"

cookie = File.read!(Path.expand("~/.erlang.cookie")) |> String.trim() |> String.to_atom()
true = Node.set_cookie(cookie)

case Node.connect(target) do
  true -> :ok
  false -> raise "cannot connect to #{inspect(target)}"
end

code = """
n = 200

payload = %{
  "language" => "python",
  "entrypoint" => "main.py",
  "files" => [%{"path" => "main.py", "content" => "print(2+2)"}],
  "limits" => %{"wall_ms" => 5000}
}

ctx = %{workspace_id: "ws_pro_demo", tier: :pro, user_id: nil}

{:ok, eff} =
  Rockbox.Settings.Pipeline.run(payload, ctx,
    audit_sink: fn _ -> :ok end,
    reserve_quota: fn _, _ -> :ok end
  )

warm = fn ->
  {:ok, vm_id} = Rockbox.Pool.Manager.acquire(eff)

  case Rockbox.VM.Server.execute_and_wait(vm_id, eff, 10_000) do
    {:ok, _msg} -> :ok
    other -> IO.inspect(other, label: "unexpected")
  end

  Rockbox.Pool.Manager.release(vm_id, eff)
end

for _ <- 1..20, do: warm.()

times =
  for _ <- 1..n do
    t0 = System.monotonic_time()
    warm.()
    System.monotonic_time() - t0
  end
  |> Enum.sort()

us = fn t -> System.convert_time_unit(t, :native, :microsecond) end
p = fn q -> Enum.at(times, min(n - 1, trunc(n * q))) |> us.() end

IO.puts(
  "vm round-trip n=\#{n} min=\#{us.(hd(times))} p50=\#{p.(0.50)} p95=\#{p.(0.95)} p99=\#{p.(0.99)} avg=\#{Float.round(us.(Enum.sum(times)) / n, 1)} µs"
)
"""

result = :rpc.block_call(target, Code, :eval_string, [code])
IO.inspect(result, label: "rpc result", limit: :infinity)
System.halt(0)
