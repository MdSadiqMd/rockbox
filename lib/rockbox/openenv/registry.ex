defmodule Rockbox.OpenEnv.Registry do
  @moduledoc """
  Named environment templates for the OpenEnv façade: `{workspace, name}` →
  Rockbox settings map (files, runtime, limits...). OpenEnv's `reset()`
  carries no program, so a trainer registers the env once and addresses it
  by name in the base URL.

  Postgres is the source of truth (`openenv_envs`); an ETS copy serves the
  per-reset lookup and is warmed from the table at boot, so registrations
  survive restarts and reset never touches the database.
  """

  use GenServer
  import Ecto.Query

  alias Rockbox.OpenEnv.Env
  alias Rockbox.Repo

  @table :rockbox_openenv_envs
  @name_re ~r/^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$/

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  def valid_name?(name) when is_binary(name), do: Regex.match?(@name_re, name)
  def valid_name?(_), do: false

  @doc "Insert or replace; written through to Postgres before the cache is updated."
  def put(workspace_id, name, %{} = settings) do
    now = DateTime.utc_now()

    {1, _} =
      Repo.insert_all(
        Env,
        [
          %{
            workspace_id: workspace_id,
            name: name,
            settings: settings,
            inserted_at: now,
            updated_at: now
          }
        ],
        on_conflict: {:replace, [:settings, :updated_at]},
        conflict_target: [:workspace_id, :name]
      )

    true = :ets.insert(@table, {{workspace_id, name}, settings})
    :ok
  end

  def fetch(workspace_id, name) do
    case :ets.lookup(@table, {workspace_id, name}) do
      [{_, settings}] ->
        {:ok, settings}

      [] ->
        case Repo.get_by(Env, workspace_id: workspace_id, name: name) do
          %Env{settings: settings} ->
            :ets.insert(@table, {{workspace_id, name}, settings})
            {:ok, settings}

          nil ->
            :error
        end
    end
  end

  def list(workspace_id) do
    Repo.all(
      from(e in Env,
        where: e.workspace_id == ^workspace_id,
        order_by: e.name,
        select: {e.name, e.settings}
      )
    )
  end

  def delete(workspace_id, name) do
    Repo.delete_all(from(e in Env, where: e.workspace_id == ^workspace_id and e.name == ^name))
    :ets.delete(@table, {workspace_id, name})
    :ok
  end

  @impl true
  def init(_) do
    :ets.new(@table, [:named_table, :public, :set, read_concurrency: true])

    for %Env{workspace_id: w, name: n, settings: s} <- Repo.all(Env) do
      :ets.insert(@table, {{w, n}, s})
    end

    {:ok, %{}}
  end
end
