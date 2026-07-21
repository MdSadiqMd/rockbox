defmodule RockboxWeb.SessionController do
  @moduledoc """
  Session lifecycle: create, run cell, destroy. Routing is sticky - every
  call for a given `session_id` lands on the same VM
  """

  use Phoenix.Controller, formats: [:json]
  alias Rockbox.{SessionRouter, VM, Pool.Manager}
  alias Rockbox.Settings.{Effective, Pipeline}

  def create(conn, %{"settings" => payload}) do
    settings = Map.put(payload, "mode", "session")

    ctx = %{
      workspace_id: conn.assigns.caller_workspace,
      tier: conn.assigns.caller_tier,
      user_id: conn.assigns.caller[:user_id]
    }

    with {:ok, %Effective{} = eff} <- Pipeline.run(settings, ctx),
         eff <- ensure_session_id(eff),
         {:ok, vm_id} <- Manager.acquire(eff),
         {:ok, _route} <- SessionRouter.register(eff.session_id, vm_id, eff.workspace_id) do
      :ok = VM.Server.execute(vm_id, eff)

      json(conn, %{
        session_id: eff.session_id,
        vm_id: vm_id,
        status: "ready",
        settings_effective: Map.delete(Map.from_struct(eff), :resolved_secrets)
      })
    else
      {:error, violations} when is_list(violations) ->
        conn |> put_status(422) |> json(%{error: "settings_invalid", violations: violations})

      {:error, reason} ->
        conn |> put_status(500) |> json(%{error: inspect(reason)})
    end
  end

  def execute(conn, %{"id" => sid, "code" => code} = params) do
    case SessionRouter.lookup(sid) do
      {:ok, route} ->
        cell = Rockbox.Wire.exec_cell(req_id(), sid, code, files: params["files"] || [])
        :ok = VM.Server.exec_cell(route.vm_id, cell)
        json(conn, %{accepted: true, vm_id: route.vm_id})

      :miss ->
        conn |> put_status(404) |> json(%{error: "session_not_found"})
    end
  end

  def delete(conn, %{"id" => sid}) do
    case SessionRouter.lookup(sid) do
      {:ok, route} ->
        VM.Supervisor.stop_vm(route.vm_id, :session_destroyed)
        SessionRouter.forget(sid)
        json(conn, %{ok: true})

      :miss ->
        conn |> put_status(404) |> json(%{error: "session_not_found"})
    end
  end

  defp ensure_session_id(%Effective{session_id: nil} = eff) do
    sid = "sess_" <> (:crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower))
    %{eff | session_id: sid}
  end

  defp ensure_session_id(eff), do: eff

  defp req_id, do: "req_" <> (:crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower))
end
