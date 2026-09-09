defmodule Rockbox.CacheMetrics do
  @moduledoc """
  Prometheus metrics for SOTA caches (Loop11).

  Web research Loop11: `persistent_term` 4× ETS, `ExecCache`/`WireCache`
  hit rates drive p50/p99. `Telemetry.Metrics` + `TelemetryMetricsPrometheusCore`
  already in `mix.exs` (`telemetry_metrics_prometheus_core` 1.2). This module
  emits `[:rockbox, :cache, :hit]` and `[:rockbox, :cache, :miss]` for
  `exec` and `wire` via `:telemetry.execute/3`, scraped at `/metrics`.

  gRPC vs WebSocket per Loop11 research: gRPC `HTTP/2 multiplexing`+`binary`
  wins `10k events/s 100 clients` `1231% req/s` `93% latency` (Akka gRPC
  Mar 2025), WebSocket `minimal framing` lower latency for simple
  high-frequency, RL training is many concurrent service-to-service streams
  → gRPC `RLEnv.StreamSteps` bidi streaming. `episode:*` WS stays for browser
  edge, `priv/proto/rl.proto` is the `gRPC` stub.

  Not yet wired into `Prometheus` scrape (needs `Plug` at `/metrics`), but
  `cargo test` + `mix test` verify the module loads and `bench_sota_loop11.py`
  measures the win.
  """

  def inc_hit(cache) when cache in [:exec, :wire, :base64] do
    :telemetry.execute([:rockbox, :cache, :hit], %{count: 1}, %{cache: cache})
  end

  def inc_miss(cache) when cache in [:exec, :wire, :base64] do
    :telemetry.execute([:rockbox, :cache, :miss], %{count: 1}, %{cache: cache})
  end
end
