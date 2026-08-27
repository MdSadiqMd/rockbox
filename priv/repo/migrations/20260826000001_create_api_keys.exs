defmodule Rockbox.Repo.Migrations.CreateApiKeys do
  use Ecto.Migration

  def change do
    create table(:api_keys, primary_key: false) do
      add :id, :binary_id, primary_key: true
      add :workspace_id, references(:workspaces, type: :string, on_delete: :delete_all),
        null: false
      add :name, :string, null: false, default: "default"
      add :prefix, :string, null: false
      add :key_hash, :binary, null: false
      add :revoked_at, :utc_datetime_usec
      add :last_used_at, :utc_datetime_usec

      timestamps(type: :utc_datetime_usec)
    end

    create unique_index(:api_keys, [:key_hash])
    create index(:api_keys, [:workspace_id])
  end
end
