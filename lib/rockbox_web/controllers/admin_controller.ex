defmodule RockboxWeb.AdminController do
  @moduledoc """
  Operator surface for sandbox-as-a-service provisioning: workspaces and API
  keys. Raw key material is returned exactly once at creation.
  """

  use Phoenix.Controller, formats: [:json]

  alias Rockbox.ApiKeys
  alias Rockbox.Workspaces

  def create_workspace(conn, params) do
    attrs = %{"name" => params["name"], "tier" => Map.get(params, "tier", "free")}

    attrs =
      Enum.reduce(["id", "concurrent_max"], attrs, fn key, acc ->
        case params[key] do
          nil -> acc
          v -> Map.put(acc, key, v)
        end
      end)

    case Workspaces.create_workspace(attrs) do
      {:ok, ws} ->
        conn |> put_status(201) |> json(workspace_view(ws))

      {:error, changeset} ->
        conn
        |> put_status(422)
        |> json(%{error: "invalid", details: format_errors(changeset)})
    end
  end

  def show_workspaces(conn, _params) do
    json(conn, %{workspaces: Enum.map(Workspaces.list_workspaces(), &workspace_view/1)})
  end

  def create_key(conn, params) do
    workspace_id = params["workspace_id"]
    name = Map.get(params, "name", "default")

    with %{} <- Workspaces.get_workspace(workspace_id) || {:error, :no_workspace},
         {:ok, %{raw: raw, key: key}} <- ApiKeys.generate(workspace_id, name: name) do
      conn
      |> put_status(201)
      |> json(%{
        id: key.id,
        name: key.name,
        prefix: key.prefix,
        workspace_id: workspace_id,
        key: raw,
        note: "store this key now — it is never shown again"
      })
    else
      {:error, :no_workspace} ->
        conn |> put_status(404) |> json(%{error: "workspace_not_found"})

      {:error, changeset} ->
        conn
        |> put_status(422)
        |> json(%{error: "invalid", details: format_errors(changeset)})
    end
  end

  def list_keys(conn, params) do
    case Workspaces.get_workspace(params["workspace_id"]) do
      nil -> conn |> put_status(404) |> json(%{error: "workspace_not_found"})
      _ws -> json(conn, %{keys: ApiKeys.list_keys(params["workspace_id"])})
    end
  end

  def revoke_key(conn, params) do
    case ApiKeys.revoke(params["workspace_id"], params["key_id"]) do
      :ok -> json(conn, %{revoked: true})
      {:error, :not_found} -> conn |> put_status(404) |> json(%{error: "not_found"})
    end
  end

  defp workspace_view(ws) do
    %{
      id: ws.id,
      name: ws.name,
      tier: ws.tier,
      concurrent_max: ws.concurrent_max,
      inserted_at: ws.inserted_at
    }
  end

  defp format_errors(changeset) do
    Ecto.Changeset.traverse_errors(changeset, fn {msg, opts} ->
      Regex.replace(~r"%{(\w+)}", msg, fn _, key ->
        opts |> Keyword.get(String.to_existing_atom(key), key) |> to_string()
      end)
    end)
  end
end
