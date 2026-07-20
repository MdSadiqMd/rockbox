defmodule Rockbox.Telemetry do
  @moduledoc """
  Telemetry supervisor + metrics catalog. The Prometheus exporter is wired up
  in [`RockboxWeb.Endpoint`] under `/metrics`
  """

  use Supervisor
  import Telemetry.Metrics

  def start_link(arg), do: Supervisor.start_link(__MODULE__, arg, name: __MODULE__)

  @impl true
  def init(_arg) do
    children = [
      {:telemetry_poller, measurements: periodic_measurements(), period: 10_000},
      {TelemetryMetricsPrometheus.Core, metrics: metrics()}
    ]

    Supervisor.init(children, strategy: :one_for_one)
  end

  def metrics do
    [
      # Settings pipeline
      counter("rockbox.settings.validated.count"),
      counter("rockbox.settings.clamped.count", tags: [:reason]),
      counter("rockbox.settings.rejected.count", tags: [:stage]),
      distribution("rockbox.settings.pipeline.duration",
        unit: {:native, :millisecond},
        reporter_options: [buckets: [0.1, 0.5, 1, 2, 5, 10, 50]]
      ),

      # VM lifecycle
      counter("rockbox.vm.spawned.count", tags: [:language, :mode]),
      counter("rockbox.vm.exited.count", tags: [:reason]),
      last_value("rockbox.pool.size", tags: [:language, :pool]),

      # Execution
      distribution("rockbox.exec.duration",
        unit: {:native, :millisecond},
        tags: [:language, :status],
        reporter_options: [buckets: [1, 5, 10, 50, 100, 500, 1_000, 5_000]]
      ),
      counter("rockbox.exec.output_capped.count"),
      counter("rockbox.exec.cost_exceeded.count"),

      # Webhook
      counter("rockbox.webhook.sent.count", tags: [:event]),
      counter("rockbox.webhook.failed.count", tags: [:event, :reason]),

      # Phoenix
      summary("phoenix.endpoint.stop.duration", unit: {:native, :millisecond}),
      summary("phoenix.router_dispatch.stop.duration",
        unit: {:native, :millisecond},
        tags: [:route]
      )
    ]
  end

  defp periodic_measurements do
    [{__MODULE__, :emit_pool_sizes, []}]
  end

  @doc false
  def emit_pool_sizes do
    # Hook expanded by Pool.Manager(kept here so the poller has a stable target)
    :ok
  end
end
