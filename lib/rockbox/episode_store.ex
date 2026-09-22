defmodule Rockbox.EpisodeStore do
  @moduledoc """
  Durable episode metadata on the shared episode volume.

  Every RL episode gets a directory under the episodes root (engine-side,
  `/var/lib/sandbox/episodes/<eid>/`) containing:

  - `manifest.json` — the frozen settings, written by the engine at start
    (secrets stripped). Any engine on this VM can rebuild + resume the worker
    from it after a crash.
  - `state.pkl` — the env's own checkpoint (`save()` payload), written by the
    sandboxed worker on its snapshot policy.

  This module is the orchestrator's window onto that directory: read a
  manifest to resurrect an episode whose VM died, and clean up on destroy.
  The app container shares the filesystem with the engines it spawns, so
  direct file access is safe here.
  """

  require Logger

  def episodes_root do
    Application.get_env(:rockbox, :episodes_root, "/var/lib/sandbox/episodes")
  end

  defp root, do: episodes_root()

  @doc "Fetch the frozen settings map for an episode. `:error` if absent/corrupt."
  @spec fetch_settings(String.t()) :: {:ok, map()} | :error
  def fetch_settings(episode_id) do
    path = Path.join([root(), episode_id, "manifest.json"])

    with true <- File.regular?(path),
         {:ok, body} <- File.read(path),
         {:ok, map} <- Jason.decode(body) do
      {:ok, map}
    else
      _ -> :error
    end
  end

  @doc """
  Clone an episode's durable state into a fresh episode directory so a new
  worker started for `child_id` resumes from the parent's checkpoint (the
  engine sees a manifest and boots in resume mode). Only the manifest and
  checkpoint are copied — user files under the volume stay with the parent.
  """
  @spec clone_episode(String.t(), String.t()) :: :ok | {:error, term()}
  def clone_episode(parent_id, child_id) do
    src = Path.join(root(), parent_id)
    dst = Path.join(root(), child_id)

    with {:ok, manifest} <- fetch_settings(parent_id),
         :ok <- File.mkdir_p(dst),
         # The engine created the parent dir 0777 so the sandboxed worker
         # (uid 65534) can write its checkpoint; the child needs the same.
         :ok <- File.chmod(dst, 0o777),
         :ok <-
           File.write(
             Path.join(dst, "manifest.json"),
             Jason.encode!(Map.put(manifest, "request_id", child_id))
           ) do
      state = Path.join(src, "state.pkl")

      if File.regular?(state) do
        case File.cp(state, Path.join(dst, "state.pkl")) do
          :ok -> :ok
          {:error, reason} -> {:error, {:copy_state, reason}}
        end
      else
        :ok
      end
    else
      :error -> {:error, :no_manifest}
      {:error, reason} -> {:error, reason}
    end
  end

  @doc """
  Remove an episode's durable state (checkpoint + manifest). Called on
  explicit client destroy — after this the episode can no longer be resumed.
  """
  @spec remove_episode(String.t()) :: :ok
  def remove_episode(episode_id) do
    dir = Path.join(root(), episode_id)

    case File.rm_rf(dir) do
      {:ok, _} ->
        :ok

      {:error, reason, _} ->
        Logger.warning("episode store: rm_rf #{dir}: #{inspect(reason)}")
        :ok
    end
  end
end
