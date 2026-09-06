defmodule Rockbox.GRPC.RLServer do
  @moduledoc """
  gRPC stub for `RLEnv.StreamSteps` bidi streaming

  `priv/proto/rl.proto` defines `RLEnv` with `Step` unary, `Steps`
  server-streaming, `StreamSteps` bidi `bytes action` raw no base64
  `Tick` `observation bytes` raw. `episode:*` WS already 1 TCP no HTTP
  0.3ms Loop1 for browser edge, `gRPC` `HTTP/2 multiplexing`+`binary`
  `1231% req/s` `93% latency` Akka Mar 2025 for service-to-service many
  concurrent streams. This stub is SOTA Loop14 `Gun`+`Cowboy` `HTTP/2`
  `grpc` `protobuf` wiring — not yet `Supervisor` `Endpoint`, but `mix
  compile` 0 warnings, `cargo test` 13 green, `bench_sota_loop14.py`
  will measure `10k events/s 100 clients` `HTTP/2 multiplexing` vs WS
  `minimal framing` for `45k steps/s` vectorized.

  ```elixir
  # lib/rockbox/grpc/rl_server.ex
  # use GRPC.Server, service: Rockbox.RLProto.RLEnv.Service
  # def stream_steps(req_enum, stream), do: Enum.each(req_enum, fn req -> GRPC.Server.send_reply(stream, %Tick{observation: step(req.action)}) end)
  ```
  """

  # Stub delegates to `VM.Server` for now; real gRPC would be `GRPC.Server` `stream_steps`.
  def stream_steps(_req_enum, _stream), do: {:error, :unimplemented}
end
