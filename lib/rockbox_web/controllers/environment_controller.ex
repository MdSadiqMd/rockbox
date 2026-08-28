defmodule RockboxWeb.EnvironmentController do
  @moduledoc """
  User-defined environments API. Create returns immediately (status
  "building"); poll the show endpoint until "ready" or "failed".
  """

  use Phoenix.Controller, formats: [:json]

  alias Rockbox.Environments

  def create(conn, params) do
    case Environments.create(conn.assigns.caller_workspace, params) do
      {:ok, env} ->
        conn |> put_status(202) |> json(Environments.view(env))

      {:error, reason} when is_map(reason) ->
        conn |> put_status(422) |> json(%{error: "invalid", details: reason})

      {:error, changeset} ->
        conn
        |> put_status(422)
        |> json(%{error: "invalid", details: format(changeset)})
    end
  end

  def show(conn, params) do
    case Environments.get(conn.assigns.caller_workspace, params["id"]) do
      nil -> conn |> put_status(404) |> json(%{error: "not_found"})
      env -> json(conn, Environments.view(env))
    end
  end

  def index(conn, _params) do
    json(conn, %{environments: Environments.list(conn.assigns.caller_workspace)})
  end

  def delete(conn, params) do
    case Environments.delete(conn.assigns.caller_workspace, params["id"]) do
      {:ok, _} -> json(conn, %{deleted: true})
      {:error, :not_found} -> conn |> put_status(404) |> json(%{error: "not_found"})
    end
  end

  defp format(changeset) do
    Ecto.Changeset.traverse_errors(changeset, fn {msg, opts} ->
      Regex.replace(~r"%{(\w+)}", msg, fn _, key ->
        opts |> Keyword.get(String.to_existing_atom(key), key) |> to_string()
      end)
    end)
  end
end
