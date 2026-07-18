defmodule Rockbox.Repo do
  @moduledoc "Ecto repo backing the audit log, workspaces, tiers, cost ledger and DLQ."

  use Ecto.Repo,
    otp_app: :rockbox,
    adapter: Ecto.Adapters.Postgres
end
