defmodule Rockbox.Repo.Migrations.CreateWebhookDlq do
  use Ecto.Migration

  def change do
    create table(:webhook_dlq) do
      add :workspace_id, :string
      add :url, :string, null: false
      add :event, :string, null: false
      add :body, :binary, null: false
      add :hmac_signature, :string
      add :attempts, :integer, null: false, default: 0
      add :last_error, :text
      add :first_failed_at, :utc_datetime_usec, null: false
      add :last_attempt_at, :utc_datetime_usec

      timestamps(type: :utc_datetime_usec)
    end

    create index(:webhook_dlq, [:workspace_id])
    create index(:webhook_dlq, [:event])
    create index(:webhook_dlq, [:first_failed_at])
  end
end
