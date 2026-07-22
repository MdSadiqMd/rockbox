defmodule Rockbox.Repo.Migrations.CreateCostEvents do
  use Ecto.Migration

  def change do
    create table(:cost_events) do
      add :request_id, :string, null: false
      add :workspace_id, :string, null: false
      add :resource, :string, null: false
      add :amount, :integer, null: false, default: 0
      add :unit, :string, null: false
      add :recorded_at, :utc_datetime_usec, null: false

      timestamps(type: :utc_datetime_usec, updated_at: false)
    end

    create index(:cost_events, [:workspace_id, :recorded_at])
    create index(:cost_events, [:request_id])
  end
end
