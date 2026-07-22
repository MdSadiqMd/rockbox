defmodule Rockbox.Repo.Migrations.CreateAuditLog do
  use Ecto.Migration

  def change do
    create table(:audit_log) do
      add :request_id, :string, null: false
      add :workspace_id, :string
      add :user_id, :string
      add :mode, :string
      add :language, :string
      add :runtime, :string
      add :settings_requested, :map, default: %{}
      add :settings_effective, :map, default: %{}
      add :clamped, {:array, :map}, default: []
      add :status, :string
      add :exit_code, :integer
      add :exec_time_ms, :integer
      add :memory_peak_mb, :integer
      add :output_truncated, :boolean, default: false
      add :credits_spent, :integer, default: 0
      add :error_summary, :text

      timestamps(type: :utc_datetime_usec, updated_at: false)
    end

    create index(:audit_log, [:request_id])
    create index(:audit_log, [:workspace_id, :inserted_at])
    create index(:audit_log, [:status])
  end
end
