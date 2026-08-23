# Generates /tmp/rb-frame.bin — one msgpack Execute command identical to what
# VM.Server sends, for engine-level benchmarks (scripts/bench-engine.sh,
# perf record). Run inside the app container:
#
#   docker compose exec app elixir -S mix run --no-start scripts/gen_frame.exs

lang = System.get_env("FRAME_LANG", "python")

code =
  case lang do
    "python" -> "print(2+2)"
    "typescript" -> "console.log(2+2)"
    "go" -> "package main\nfunc main(){println(2+2)}"
    "rust" -> "fn main(){println!(\"{}\", 2+2)}"
    "cpp" -> "#include <cstdio>\nint main(){std::printf(\"4\\n\");}"
  end

ext = if lang == "typescript", do: "ts", else: lang

payload_map = %{
  "language" => lang,
  "entrypoint" => "main.#{ext}",
  "files" => [%{"path" => "main.#{ext}", "content" => code}],
  "limits" => %{"wall_ms" => 5000}
}

ctx = %{workspace_id: "ws_pro_demo", tier: :pro, user_id: nil}

{:ok, eff} =
  Rockbox.Settings.Pipeline.run(payload_map, ctx,
    audit_sink: fn _ -> :ok end,
    reserve_quota: fn _, _ -> :ok end
  )

wire =
  Map.merge(%{"cmd" => "execute"}, Rockbox.Settings.Effective.to_wire(eff))
  |> Msgpax.pack!(iodata: true)
  |> IO.iodata_to_binary()

# FrameReader expects a 4-byte big-endian length prefix before each payload.
frame = <<byte_size(wire)::unsigned-big-integer-size(32), wire::binary>>
File.write!("/tmp/rb-frame.bin", frame)
IO.puts("wrote /tmp/rb-frame.bin (#{byte_size(frame)} bytes, lang=#{lang})")
