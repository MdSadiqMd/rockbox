defmodule Rockbox.Repo.Migrations.CreateWorkspaces do
  use Ecto.Migration

  def change do
    create table(:workspaces, primary_key: false) do
      add :id, :string, primary_key: true
      add :name, :string, null: false
      add :tier, :string, null: false, default: "free"
      add :default_settings, :map, null: false, default: %{}
      add :concurrent_max, :integer, null: false, default: 5
      add :webhook_url, :string
      add :webhook_secret_ref, :string

      timestamps(type: :utc_datetime_usec)
    end

    create unique_index(:workspaces, [:name])
  end
end
