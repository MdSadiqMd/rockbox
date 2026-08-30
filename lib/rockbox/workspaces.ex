defmodule Rockbox.Workspaces do
  @moduledoc """
  Workspace registry. A workspace owns sandboxes, quota and API keys; its
  `tier` selects the ceiling set the settings pipeline clamps against.

  The table existed since day one but nothing read or wrote it — auth was a
  parseable dev token. This context is the minimal registry surface needed
  for key issuance and tier resolution.
  """

  import Ecto.Query
  alias Ecto.Changeset
  alias Rockbox.Repo

  defmodule Workspace do
    use Ecto.Schema

    @primary_key {:id, :string, autogenerate: false}
    @foreign_key_type :binary_id

    schema "workspaces" do
      field(:name, :string)
      field(:tier, :string, default: "free")
      field(:default_settings, :map, default: %{})
      field(:concurrent_max, :integer, default: 5)
      field(:webhook_url, :string)
      field(:webhook_secret_ref, :string)

      timestamps(type: :utc_datetime_usec)
    end

    def changeset(ws, attrs) do
      ws
      |> Changeset.cast(attrs, [:id, :name, :tier, :default_settings, :concurrent_max])
      |> Changeset.validate_required([:name, :tier])
      |> Changeset.validate_inclusion(:tier, ["free", "pro", "enterprise"])
      |> Changeset.unique_constraint(:id)
      |> Changeset.unique_constraint(:name)
    end
  end

  def get_workspace(id), do: Repo.get(Workspace, id)

  def get_workspace!(id), do: Repo.get!(Workspace, id)

  @doc "Creates a workspace. Pass `id:` to pin a stable identifier (used by seeded demo workspaces)."
  def create_workspace(attrs) do
    %Workspace{}
    |> Workspace.changeset(attrs)
    |> Repo.insert()
  end

  def list_workspaces do
    Repo.all(from(w in Workspace, order_by: [asc: w.name]))
  end
end
