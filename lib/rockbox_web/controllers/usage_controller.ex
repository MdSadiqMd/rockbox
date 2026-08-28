defmodule RockboxWeb.UsageController do
  @moduledoc """
  Per-workspace usage metering — the billing surface for sandbox-as-a-service.
  Counters are lock-free ETS atomics updated on the hot paths (request
  reserve + RL step metering); reads never touch a GenServer.
  """

  use Phoenix.Controller, formats: [:json]

  alias Rockbox.QuotaTracker

  def show(conn, _params) do
    json(conn, QuotaTracker.usage(conn.assigns.caller_workspace))
  end
end
