# Stage-level attribution for the orchestrator hot path. Pure functions only,
# so this runs in a throwaway BEAM (`mix run --no-start`) against the same code
# the live app executes:
#
#   MIX_ENV=bench docker compose exec app elixir -S mix run --no-start scripts/profile_stages.exs
#
# Reports µs/op percentiles for each stage of POST /api/execute that is pure
# CPU on the BEAM: settings pipeline, wire encode, response JSON encode.

defmodule ProfileStages do
  @moduledoc false

  @payload %{
    "language" => "python",
    "entrypoint" => "main.py",
    "files" => [%{"path" => "main.py", "content" => "print(2+2)"}],
    "limits" => %{"wall_ms" => 5000}
  }

  @ctx %{workspace_id: "ws_pro_demo", tier: :pro, user_id: nil}

  defp bench(label, n, fun) do
    for _ <- 1..max(div(n, 10), 10), do: fun.()

    times =
      for _ <- 1..n do
        t0 = System.monotonic_time()
        fun.()
        System.monotonic_time() - t0
      end
      |> Enum.sort()

    us = fn t -> System.convert_time_unit(t, :native, :microsecond) end
    p = fn q -> Enum.at(times, min(n - 1, trunc(n * q))) |> us.() end
    avg = us.(Enum.sum(times)) / n

    IO.puts(
      "#{label}\n  n=#{n} min=#{us.(hd(times))} p50=#{p.(0.50)} p95=#{p.(0.95)} p99=#{p.(0.99)} avg=#{Float.round(avg, 2)} µs"
    )
  end

  def run do
    {:ok, _} = Application.ensure_all_started(:rockbox)

    IO.puts("== stage attribution (pure BEAM CPU cost) ==\n")

    pipeline_fn = fn ->
      Rockbox.Settings.Pipeline.run(@payload, @ctx,
        audit_sink: fn _ -> :ok end,
        reserve_quota: fn _, _ -> :ok end
      )
    end

    {:ok, %Rockbox.Settings.Effective{} = eff} = pipeline_fn.()

    bench("Rockbox.Settings.Pipeline.run (14 steps -> Effective)", 2000, pipeline_fn)

    wire_map = Rockbox.Settings.Effective.to_wire(eff)

    bench("Effective.to_wire", 5000, fn -> Rockbox.Settings.Effective.to_wire(eff) end)

    bench("Msgpax.pack!(execute cmd)", 5000, fn ->
      Msgpax.pack!(Map.merge(%{"cmd" => "execute"}, wire_map), iodata: true)
    end)

    encoded = Msgpax.pack!(Map.merge(%{"cmd" => "execute"}, wire_map), iodata: true)
    bin = IO.iodata_to_binary(encoded)

    bench("Wire.decode (result frame)", 5000, fn ->
      Rockbox.Wire.decode(result_frame())
    end)

    bench("Msgpax.pack! full frame (#{byte_size(bin)}B)", 5000, fn ->
      Msgpax.pack!(Map.merge(%{"cmd" => "execute"}, wire_map), iodata: true)
    end)

    resp = %{
      status: "success",
      request_id: eff.request_id,
      vm_id: "vm_test",
      session_id: nil,
      exit_code: 0,
      output: "4\n",
      errors: "",
      exec_time_ms: 5,
      memory_peak_mb: 9,
      output_truncated: false,
      settings_effective: Map.delete(Map.from_struct(eff), :resolved_secrets),
      clamped: [],
      warnings: []
    }

    bench("Jason.encode! (response body)", 2000, fn -> Jason.encode!(resp) end)

    :ok
  end

  defp result_frame do
    # A realistic engine result frame (msgpack-encoded map), pre-packed once.
    frame = %{
      "type" => "result",
      "request_id" => "req_abcd1234abcd1234",
      "status" => "success",
      "exit_code" => 0,
      "exec_time_ms" => 5,
      "memory_peak_mb" => 9,
      "cpu_time_ms" => 0,
      "output_bytes" => 2,
      "output_truncated" => false,
      "output" => "4\n",
      "errors" => ""
    }

    frame |> Msgpax.pack!(iodata: false)
  end
end


ProfileStages.run()
