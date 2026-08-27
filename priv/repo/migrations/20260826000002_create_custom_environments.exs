defmodule Rockbox.Repo.Migrations.CreateCustomEnvironments do
  use Ecto.Migration

  def change do
    create table(:custom_environments, primary_key: false) do
      add :id, :binary_id, primary_key: true
      add :workspace_id, references(:workspaces, type: :string, on_delete: :delete_all),
        null: false

      add :name, :string, null: false
      add :language, :string, null: false
      # User intent: %{"kind" => "python_packages", "packages" => [...]} or
      # %{"kind" => "flake", "flake_nix" => "<verbatim source>"}
      add :spec, :map, null: false
      add :status, :string, null: false, default: "building"
      add :store_path, :string
      add :bin_path, :string
      add :lock_hash, :binary
      add :error, :text

      timestamps(type: :utc_datetime_usec)
    end

    create index(:custom_environments, [:workspace_id])
    create unique_index(:custom_environments, [:workspace_id, :name])
  end
end
