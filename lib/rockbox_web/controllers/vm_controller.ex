defmodule RockboxWeb.VMController do
  @moduledoc "Direct VM lifecycle endpoints. Useful for hot pool management"

  use Phoenix.Controller, formats: [:json]
  alias Rockbox.{VM, Pool.Manager}
  alias Rockbox.Settings.{Effective, Pipeline}

  def index(conn, _params) do
    rows =
      VM.Registry.list()
      |> Enum.map(fn {vm_id, _pid} ->
        case VM.Server.status(vm_id) do
          {:error, _} -> %{vm_id: vm_id, status: "down"}
          s -> s
        end
      end)

    json(conn, %{vms: rows})
  end

  def show(conn, %{"id" => vm_id}) do
    case VM.Server.status(vm_id) do
      {:error, :not_found} -> conn |> put_status(404) |> json(%{error: "not_found"})
      status -> json(conn, status)
    end
  end

  def create(conn, %{"settings" => payload}) do
    ctx = caller_ctx(conn)

    case Pipeline.run(payload, ctx) do
      {:ok, %Effective{} = eff} ->
        case Manager.acquire(eff) do
          {:ok, vm_id} -> json(conn, %{vm_id: vm_id})
          {:error, reason} -> conn |> put_status(500) |> json(%{error: inspect(reason)})
        end

      {:error, violations} ->
        conn |> put_status(422) |> json(%{error: "settings_invalid", violations: violations})
    end
  end

  def execute(conn, %{"id" => vm_id, "settings" => payload}) do
    ctx = caller_ctx(conn)

    case Pipeline.run(payload, ctx) do
      {:ok, eff} ->
        :ok = VM.Server.execute(vm_id, eff)
        json(conn, %{accepted: true, request_id: eff.request_id})

      {:error, violations} ->
        conn |> put_status(422) |> json(%{error: "settings_invalid", violations: violations})
    end
  end

  def delete(conn, %{"id" => vm_id}) do
    VM.Supervisor.stop_vm(vm_id)
    json(conn, %{ok: true})
  end

  defp caller_ctx(conn),
    do: %{
      workspace_id: conn.assigns.caller_workspace,
      tier: conn.assigns.caller_tier,
      user_id: conn.assigns.caller[:user_id]
    }
end
