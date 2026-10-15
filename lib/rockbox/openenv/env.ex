defmodule Rockbox.OpenEnv.Env do
  @moduledoc "A named OpenEnv environment template: `{workspace, name}` → Rockbox RL settings."

  use Ecto.Schema

  @primary_key false
  schema "openenv_envs" do
    field(:workspace_id, :string, primary_key: true)
    field(:name, :string, primary_key: true)
    field(:settings, :map, default: %{})

    timestamps(type: :utc_datetime_usec)
  end
end
