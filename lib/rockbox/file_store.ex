defmodule Rockbox.FileStore do
  @moduledoc """
  Orchestrator-side access to sandbox volumes that live on the shared host
  filesystem: session roots (`/var/lib/sandbox/sessions/<sid>/`, mounted RW at
  `/session`) and episode roots (`/var/lib/sandbox/episodes/<eid>/`, mounted
  at `/episode`). Same trust model as `EpisodeStore`: the app container and
  the engines it spawns share a filesystem, so the orchestrator can serve
  files without a round trip through the engine protocol.

  Every operation is path-contained: user-supplied paths are canonicalised and
  must remain inside the root, including through symlink chains (bounded
  resolution). Reads are capped (`@max_read_bytes`) so a runaway program
  cannot OOM the orchestrator via a single GET.
  """

  require Logger

  @max_read_bytes 8 * 1024 * 1024
  @max_write_bytes 16 * 1024 * 1024
  @max_list_entries 10_000
  @max_symlink_depth 8
  @sessions_root "/var/lib/sandbox/sessions"

  def max_read_bytes, do: @max_read_bytes
  def max_write_bytes, do: @max_write_bytes

  @doc "Resolve the root dir for a session. `{:error, :not_found}` if absent."
  @spec session_root(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def session_root(session_id) do
    root = Path.join(@sessions_root, session_id)
    if File.dir?(root), do: {:ok, root}, else: {:error, :not_found}
  end

  @doc "Resolve the root dir for an episode."
  @spec episode_root(String.t()) :: {:ok, String.t()} | {:error, :not_found}
  def episode_root(episode_id) do
    root = Path.join(Rockbox.EpisodeStore.episodes_root(), episode_id)
    if File.dir?(root), do: {:ok, root}, else: {:error, :not_found}
  end

  @doc """
  Contain a user path under `root`. Rejects `..` traversal and paths that
  resolve outside root. Returns an absolute filesystem path.
  """
  @spec safe_path(String.t(), String.t()) ::
          {:ok, String.t()} | {:error, :invalid_path | :outside_root}
  def safe_path(root, user_path) when is_binary(user_path) do
    trimmed = String.trim_leading(user_path, "/")

    cond do
      trimmed == "" ->
        {:ok, root}

      ".." in Path.split(trimmed) ->
        {:error, :invalid_path}

      true ->
        joined = Path.expand(Path.join(root, trimmed))

        if inside?(root, joined), do: {:ok, joined}, else: {:error, :outside_root}
    end
  end

  def safe_path(_root, _other), do: {:error, :invalid_path}

  defp inside?(root, p), do: p == root or String.starts_with?(p, root <> "/")

  # Verify a path stays inside the sandbox volume. Symlink chains are followed
  # (bounded) and the final target re-checked, so links pointing out of the
  # volume are refused. Paths that do not exist yet (writes into new dirs) are
  # validated against their nearest existing ancestor instead.
  defp contained(root, path, depth \\ 0)

  defp contained(_root, _path, depth) when depth > @max_symlink_depth,
    do: {:error, :too_many_symlinks}

  defp contained(root, path, depth) do
    case symlink_target(path) do
      {:link, target} ->
        resolved = Path.expand(target, Path.dirname(path))

        if inside?(root, resolved) do
          contained(root, resolved, depth + 1)
        else
          {:error, :outside_root}
        end

      # Not a symlink (or absent — an ancestor check catches escapes there).
      :not_symlink ->
        if not inside?(root, Path.expand(path)) do
          {:error, :outside_root}
        else
          case :file.read_file_info(String.to_charlist(path)) do
            {:ok, _} ->
              {:ok, path}

            {:error, :enoent} ->
              with {:ok, _nearest_existing} <- ancestors_contained?(root, path, depth) do
                {:ok, path}
              end

            {:error, reason} ->
              {:error, reason}
          end
        end

      {:error, reason} ->
        {:error, reason}
    end
  end

  defp symlink_target(path) do
    case :file.read_link(String.to_charlist(path)) do
      {:ok, target} -> {:link, List.to_string(target)}
      {:error, r} when r in [:einval, :enoent] -> :not_symlink
      {:error, reason} -> {:error, reason}
    end
  end

  # Walk up to the nearest existing component of a missing path and verify its
  # containment, so a symlinked ancestor cannot smuggle writes outside.
  defp ancestors_contained?(root, path, depth) do
    parent = Path.dirname(path)

    cond do
      parent == path -> {:error, :outside_root}
      parent == Path.expand(root) -> contained(root, parent, depth + 1)
      true -> contained(root, parent, depth + 1)
    end
  end

  @doc "List a directory relative to root: entries with type/size/mtime."
  @spec list(String.t(), String.t()) :: {:ok, [map()]} | {:error, atom()}
  def list(root, user_path) do
    with {:ok, dir} <- safe_path(root, user_path),
         {:ok, dir} <- contained(root, dir),
         true <- File.dir?(dir) do
      entries =
        dir
        |> File.ls!()
        |> Enum.sort()
        |> Enum.take(@max_list_entries)
        |> Enum.map(fn name ->
          full = Path.join(dir, name)
          stat = File.stat!(full, time: :posix)

          %{
            name: name,
            path: "/" <> Path.relative_to(full, root),
            type: to_string(stat.type),
            size: stat.size,
            mtime: stat.mtime
          }
        end)

      {:ok, entries}
    else
      false -> {:error, :enoent}
      error -> error
    end
  rescue
    e in [File.Error] -> {:error, e.reason}
  end

  @doc "Read a file relative to root, base64-encoded, capped at max_read_bytes."
  @spec read(String.t(), String.t()) ::
          {:ok,
           %{
             path: String.t(),
             size: non_neg_integer(),
             content_b64: binary(),
             truncated: boolean()
           }}
          | {:error, atom() | {:is_not_regular, String.t()}}
  def read(root, user_path) do
    with {:ok, path} <- safe_path(root, user_path),
         {:ok, path} <- contained(root, path),
         {:ok, %File.Stat{size: size, type: :regular}} <- File.stat(path),
         {:ok, body} <-
           if(size > @max_read_bytes,
             do: read_prefix(path, @max_read_bytes),
             else: File.read(path)
           ) do
      {:ok,
       %{
         path: "/" <> Path.relative_to(path, root),
         size: size,
         content_b64: Base.encode64(body),
         truncated: size > @max_read_bytes
       }}
    else
      {:ok, %File.Stat{type: other}} -> {:error, {:is_not_regular, to_string(other)}}
      {:error, reason} -> {:error, reason}
    end
  end

  defp read_prefix(path, n) do
    File.open(path, [:read, :raw], fn dev -> IO.binread(dev, n) end)
  end

  @doc "Write (or overwrite) a file relative to root from base64 content."
  @spec write(String.t(), String.t(), binary()) ::
          {:ok, %{path: String.t(), size: non_neg_integer()}} | {:error, atom()}
  def write(root, user_path, content_b64) when byte_size(content_b64) <= @max_write_bytes do
    with {:ok, path} <- safe_path(root, user_path),
         {:ok, path} <- contained(root, path),
         {:ok, bin} <- Base.decode64(content_b64),
         :ok <- File.mkdir_p(Path.dirname(path)),
         :ok <- File.write(path, bin) do
      {:ok, %{path: "/" <> Path.relative_to(path, root), size: byte_size(bin)}}
    else
      :error -> {:error, :invalid_base64}
      {:error, reason} -> {:error, reason}
    end
  end

  def write(_root, _path, _too_big), do: {:error, :payload_too_large}

  @doc "Delete a file or directory tree relative to root."
  @spec remove(String.t(), String.t()) :: {:ok, %{deleted: String.t()}} | {:error, atom()}
  def remove(root, user_path) do
    with {:ok, path} <- safe_path(root, user_path),
         {:ok, path} <- contained(root, path),
         false <- Path.relative_to(path, root) == "." do
      case File.rm_rf(path) do
        {:ok, _} -> {:ok, %{deleted: "/" <> Path.relative_to(path, root)}}
        {:error, reason, _} -> {:error, reason}
      end
    else
      true -> {:error, :cannot_remove_root}
      error -> error
    end
  end
end
