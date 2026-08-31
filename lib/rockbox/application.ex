defmodule Rockbox.Application do
  @moduledoc """
  Top-level OTP application + supervision tree for Rockbox

  Children are grouped by responsibility and the tree is intentionally flat
  sub-supervisors are only used where tight coupling demands `:rest_for_one` or `:one_for_all`
  """

  use Application

  @impl true
  def start(_type, _args) do
    children =
      [
        # Telemetry / observability — start first so subsequent boot is observable
        Rockbox.Telemetry,

        # Persistence
        Rockbox.Repo,

        # PubSub for VM events + audit fanout
        {Phoenix.PubSub, name: Rockbox.PubSub},

        # Cross-cutting services
        Rockbox.QuotaTracker,
        Rockbox.SecretsBroker,
        Rockbox.AuditLog,
        Rockbox.WebhookDispatcher,
        Rockbox.SessionRouter,
        Rockbox.EpisodeRegistry,
        Rockbox.ApiKeys.Cache,
        Rockbox.Environments.Builder,

        # VM lifecycle
        Rockbox.VM.Registry,
        Rockbox.VM.Supervisor,

        # Pool + Autoscaler
        Rockbox.Pool.Manager,
        Rockbox.Pool.Autoscaler,

        # HTTP/WS endpoint last (only accepts traffic once the rest is up)
        RockboxWeb.Endpoint
      ]
      |> Enum.reject(&is_nil/1)

    opts = [strategy: :one_for_one, name: Rockbox.Supervisor]
    Supervisor.start_link(children, opts)
  end

  @impl true
  def config_change(changed, _new, removed) do
    RockboxWeb.Endpoint.config_change(changed, removed)
    :ok
  end
end
