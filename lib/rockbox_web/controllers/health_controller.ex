defmodule RockboxWeb.HealthController do
  @moduledoc "Liveness + readiness probes for Kubernetes"

  use Phoenix.Controller, formats: [:json]

  def show(conn, _), do: json(conn, %{status: "ok"})

  def ready(conn, _) do
    # Optional: probe Repo + Pool + Engine binary presence.
    json(conn, %{
      status: "ready",
      engine_binary: Application.get_env(:rockbox, :engine)[:binary],
      vms: length(Rockbox.VM.Registry.list())
    })
  end
end
