defmodule RockboxWeb.FileController do
  @moduledoc """
  File operations on sandbox volumes: `/api/sessions/:id/files` and
  `/api/rl/episodes/:episode_id/files`.

  - `GET  .../files?path=/dir`        → directory listing
  - `GET  .../files/content?path=...` → file content (JSON, base64)
  - `PUT  .../files` {path, content}  → write file (base64)
  - `DELETE .../files?path=...`       → remove file/dir

  Ownership: the session/episode must belong to the caller's workspace. All
  paths are contained to the volume by `Rockbox.FileStore`.
  """

  use Phoenix.Controller, formats: [:json]
  alias Rockbox.{EpisodeRegistry, EpisodeStore, FileStore, SessionRouter}

  def show(conn, %{"id" => sid} = params),
    do: session_op(conn, sid, fn root -> volume_op(conn, root, params) end)

  def show(conn, %{"episode_id" => eid} = params),
    do: episode_op(conn, eid, fn root -> volume_op(conn, root, params) end)

  def content(conn, %{"id" => sid} = params),
    do:
      session_op(conn, sid, fn root -> store_op(conn, FileStore.read(root, path_of(params))) end)

  def content(conn, %{"episode_id" => eid} = params),
    do:
      episode_op(conn, eid, fn root -> store_op(conn, FileStore.read(root, path_of(params))) end)

  def write(conn, %{"id" => sid} = params),
    do:
      session_op(conn, sid, fn root ->
        store_op(conn, FileStore.write(root, path_of(params), content_of(params)))
      end)

  def write(conn, %{"episode_id" => eid} = params),
    do:
      episode_op(conn, eid, fn root ->
        store_op(conn, FileStore.write(root, path_of(params), content_of(params)))
      end)

  def delete(conn, %{"id" => sid} = params),
    do:
      session_op(conn, sid, fn root -> store_op(conn, FileStore.remove(root, path_of(params))) end)

  def delete(conn, %{"episode_id" => eid} = params),
    do:
      episode_op(conn, eid, fn root -> store_op(conn, FileStore.remove(root, path_of(params))) end)

  # -- authorization + root resolution --------------------------------------

  # The engine materialises the volume dir asynchronously after create (hot
  # pool hit ~ms, cold VM spawn up to seconds), so poll briefly instead of
  # 404-ing on a just-created id.
  @ready_wait_ms 10_000
  @ready_poll_ms 25

  defp session_op(conn, sid, fun) do
    case SessionRouter.lookup(sid) do
      {:ok, %{workspace_id: wid}} when wid == conn.assigns.caller_workspace ->
        with_root(conn, sid, &FileStore.session_root/1, fun)

      {:ok, _other_workspace} ->
        forbidden(conn)

      :miss ->
        gone(conn)
    end
  end

  defp episode_op(conn, eid, fun) do
    case EpisodeRegistry.lookup_route(eid) do
      {:ok, _vm, wid, _tier} when wid == conn.assigns.caller_workspace ->
        with_root(conn, eid, &FileStore.episode_root/1, fun)

      {:ok, _vm, _other_workspace, _tier} ->
        forbidden(conn)

      :miss ->
        # Durable episodes survive VM loss; the manifest pins their owner.
        case {EpisodeStore.fetch_settings(eid), FileStore.episode_root(eid)} do
          {{:ok, %{"workspace_id" => wid}}, {:ok, root}}
          when wid == conn.assigns.caller_workspace ->
            fun.(root)

          _ ->
            gone(conn)
        end
    end
  end

  defp with_root(conn, id, resolver, fun) do
    case await_root(id, resolver, System.monotonic_time(:millisecond)) do
      {:ok, root} -> fun.(root)
      {:error, :not_found} -> not_ready(conn)
      {:error, reason} -> conn |> put_status(500) |> json(%{error: inspect(reason)})
    end
  end

  defp await_root(id, resolver, t0) do
    case resolver.(id) do
      {:ok, root} ->
        {:ok, root}

      {:error, :not_found} = miss ->
        if System.monotonic_time(:millisecond) - t0 < @ready_wait_ms do
          Process.sleep(@ready_poll_ms)
          await_root(id, resolver, t0)
        else
          miss
        end

      other ->
        other
    end
  end

  # -- volume operations -----------------------------------------------------

  defp volume_op(conn, root, params),
    do: store_op(conn, FileStore.list(root, path_of(params)))

  defp store_op(conn, {:ok, result}), do: json(conn, result)

  defp store_op(conn, {:error, {:is_not_regular, type}}),
    do: conn |> put_status(409) |> json(%{error: "not_a_regular_file", type: type})

  defp store_op(conn, {:error, reason}) when is_atom(reason), do: file_error(conn, reason)

  defp store_op(conn, {:error, reason}),
    do: conn |> put_status(400) |> json(%{error: "file_error", reason: inspect(reason)})

  defp file_error(conn, :enoent),
    do: conn |> put_status(404) |> json(%{error: "not_found", reason: :enoent})

  defp file_error(conn, :outside_root),
    do: conn |> put_status(400) |> json(%{error: "path_outside_volume"})

  defp file_error(conn, :invalid_path),
    do: conn |> put_status(400) |> json(%{error: "invalid_path"})

  defp file_error(conn, :payload_too_large),
    do:
      conn
      |> put_status(413)
      |> json(%{error: "payload_too_large", max_bytes: FileStore.max_write_bytes()})

  defp file_error(conn, :cannot_remove_root),
    do: conn |> put_status(400) |> json(%{error: "cannot_remove_root"})

  defp file_error(conn, :invalid_base64),
    do: conn |> put_status(400) |> json(%{error: "invalid_base64"})

  defp file_error(conn, reason),
    do: conn |> put_status(400) |> json(%{error: "file_error", reason: inspect(reason)})

  defp forbidden(conn), do: conn |> put_status(403) |> json(%{error: "forbidden"})
  defp gone(conn), do: conn |> put_status(404) |> json(%{error: "not_found"})

  defp not_ready(conn),
    do: conn |> put_status(409) |> json(%{error: "volume_not_ready_fc"})

  defp path_of(params), do: params["path"] || "/"
  defp content_of(params), do: params["content"] || ""
end
