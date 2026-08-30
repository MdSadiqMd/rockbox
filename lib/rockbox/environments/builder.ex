defmodule Rockbox.Environments.Builder do
  @moduledoc """
  Single-flight Nix build worker for user-defined environments.

  One build at a time (the GenServer itself is the mutex — casts queue in
  its mailbox), each build bounded by a wall-clock timeout. On success the
  resulting interpreter path is pinned into a JSON descriptor under the
  engine's custom-runtime directory; engines pick it up lazily per lookup,
  so no restart or protocol change is involved.
  """

  use GenServer
  require Logger

  alias Rockbox.Environments.Environment
  alias Rockbox.Repo

  @build_timeout_ms 15 * 60 * 1000
  @id_re ~r/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  def enqueue(env_id), do: GenServer.cast(__MODULE__, {:enqueue, env_id})

  @doc "Engine-visible runtime name for an environment id."
  def runtime_name(env_id), do: "custom-" <> env_id

  @doc "Extract the environment id from a runtime name, or nil if not custom."
  def parse_runtime_name("custom-" <> rest) do
    if Regex.match?(@id_re, rest), do: rest
  end

  def parse_runtime_name(_), do: nil

  def unregister(%Environment{id: id}) do
    File.rm(descriptor_path(id))
    :ok
  end

  def descriptor_path(env_id), do: Path.join(runtimes_dir(), "#{runtime_name(env_id)}.json")

  def runtimes_dir,
    do: Application.get_env(:rockbox, :custom_runtimes_dir, "/etc/sandbox/custom-runtimes")

  defp envs_root,
    do: Application.get_env(:rockbox, :environments_root, "/var/lib/rockbox/envs")

  defp nix_bin,
    do: System.find_executable("nix") || "/nix/var/nix/profiles/default/bin/nix"

  @impl true
  def init(state) do
    sweep_orphans()
    {:ok, state}
  end

  @impl true
  def handle_cast({:enqueue, env_id}, state) do
    Process.send(self(), :next, [])
    {:noreply, Map.put(state, env_id, :queued)}
  end

  @impl true
  def handle_info(:next, state) do
    case Enum.find(state, fn {_id, st} -> st == :queued end) do
      {env_id, :queued} ->
        state = Map.put(state, env_id, :building)
        _result = run_build(env_id)
        state = Map.delete(state, env_id)

        unless map_size(state) == 0 do
          Process.send(self(), :next, [])
        end

        {:noreply, state}

      _ ->
        {:noreply, state}
    end
  end

  def handle_info(_, state), do: {:noreply, state}

  defp run_build(env_id) do
    case Repo.get(Environment, env_id) do
      nil ->
        :ok

      %Environment{status: "ready"} = env ->
        # Idempotent rebuild guard: descriptor already present?
        if File.exists?(descriptor_path(env.id)), do: :ok, else: build(env)

      env ->
        build(env)
    end
  end

  defp build(env) do
    started = System.monotonic_time()

    result =
      with {:ok, dir} <- write_flake(env),
           :ok <- nix_build(dir),
           {:ok, out_path} <- read_out_link(dir),
           {:ok, bin_path} <- locate_interpreter(out_path),
           :ok <- write_descriptor(dir, env, out_path, bin_path) do
        :ok
      else
        {:error, reason} -> {:error, reason}
      end

    duration_ms =
      System.convert_time_unit(System.monotonic_time() - started, :native, :millisecond)

    case result do
      :ok ->
        env |> Ecto.Changeset.change(status: "ready", error: nil) |> Repo.update!()
        Logger.info("custom env #{env.id} built in #{duration_ms}ms")

      {:error, reason} ->
        env
        |> Ecto.Changeset.change(status: "failed", error: stringify(reason))
        |> Repo.update!()

        File.rm(descriptor_path(env.id))
        Logger.warning("custom env #{env.id} build failed: #{stringify(reason)}")
    end

    :ok
  end

  defp write_flake(env) do
    dir = Path.join(envs_root(), env.id)
    File.mkdir_p!(dir)

    source =
      case env.spec do
        %{"kind" => "python_packages", "packages" => pkgs} -> python_flake(pkgs)
        %{"kind" => "flake", "flake_nix" => src} -> src
      end

    File.write!(Path.join(dir, "flake.nix"), source)
    {:ok, dir}
  rescue
    e -> {:error, "write_flake: #{Exception.message(e)}"}
  end

  defp nix_build(dir) do
    args = [
      "--extra-experimental-features",
      "nix-command flakes",
      "build",
      "--out-link",
      Path.join(dir, "result"),
      "#{dir}#default"
    ]

    # System.cmd has no timeout option; drive the port by hand so a wedged
    # build can't pin the single-flight queue forever.
    port =
      Port.open(
        {:spawn_executable, nix_bin()},
        [:binary, :exit_status, :hide, :use_stdio, :stderr_to_stdout, line: 4096, args: args]
      )

    deadline = System.monotonic_time(:millisecond) + @build_timeout_ms
    collect(port, deadline, [])
  end

  defp collect(port, deadline, acc) do
    now = System.monotonic_time(:millisecond)
    remaining = max(deadline - now, 0)

    receive do
      {^port, {:data, data}} ->
        # line-mode ports wrap chunks as {:eol, line} | binary
        chunk =
          case data do
            {:eol, line} -> IO.iodata_to_binary([line, "\n"])
            bin when is_binary(bin) -> bin
          end

        collect(port, deadline, [chunk | acc])

      {^port, {:exit_status, 0}} ->
        :ok

      {^port, {:exit_status, code}} ->
        {:error,
         "nix build exit #{code}: #{last_lines(IO.iodata_to_binary(Enum.reverse(acc)), 20)}"}
    after
      remaining ->
        Port.close(port)
        {:error, "nix build timed out after #{@build_timeout_ms}ms"}
    end
  end

  defp read_out_link(dir) do
    link = Path.join(dir, "result")

    case File.read_link(link) do
      {:ok, target} ->
        {:ok, resolve_link(dir, target)}

      _ ->
        {:error, "no result symlink after build"}
    end
  end

  defp locate_interpreter(store_path) do
    candidates =
      case store_path_language_hint(store_path) do
        "python" -> ["python3"]
        _ -> ["node", "bun", "go", "rustc", "g++", "main"]
      end

    Enum.find_value(candidates, :error, fn name ->
      p = Path.join([store_path, "bin", name])

      if File.exists?(p) and File.exists?(Path.join(store_path, "bin")),
        do: {:ok, canonical(p)},
        else: nil
    end)
    |> case do
      {:ok, bin} -> {:ok, bin}
      :error -> {:error, "interpreter not found in #{store_path}/bin"}
    end
  end

  defp store_path_language_hint(path) do
    if String.contains?(Path.basename(path), "python"), do: "python", else: "generic"
  end

  defp canonical(p), do: p

  defp resolve_link(dir, target) do
    if Path.type(target) == :absolute, do: target, else: Path.expand(target, dir)
  end

  defp write_descriptor(dir, env, out_path, bin_path) do
    lock_hash = lock_digest(dir)
    File.mkdir_p!(runtimes_dir())

    descriptor = %{
      language: env.language,
      executable: Path.basename(bin_path),
      bin: bin_path,
      env: %{},
      interpreter_flags: [],
      lock_hex: hex(lock_hash)
    }

    json = Jason.encode!(descriptor)

    tmp = descriptor_path(env.id) <> ".tmp"
    File.write!(tmp, json)
    File.rename(tmp, descriptor_path(env.id))

    env
    |> Ecto.Changeset.change(store_path: out_path, bin_path: bin_path, lock_hash: lock_hash)
    |> Repo.update!()

    :ok
  rescue
    e -> {:error, "write_descriptor: #{Exception.message(e)}"}
  end

  defp lock_digest(dir) do
    case File.read(Path.join(dir, "flake.lock")) do
      {:ok, bytes} -> :crypto.hash(:sha256, bytes)
      _ -> nil
    end
  end

  defp hex(nil), do: nil
  defp hex(bytes) when is_binary(bytes), do: Base.encode16(bytes, case: :lower)

  defp python_flake(pkgs) when is_list(pkgs) do
    # nix attribute names — bare identifiers, whitespace-separated
    attr_list = Enum.join(pkgs, " ")

    """
    {
      inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
      outputs = { self, nixpkgs }:
        let
          systems = [ "aarch64-linux" "x86_64-linux" ];
          forAll = f: builtins.listToAttrs (map (s: { name = s; value = f s; }) systems);
          mkEnv = system:
            let pkgs = import nixpkgs { inherit system; };
            in pkgs.python313.withPackages (ps: with ps; [ #{attr_list} ]);
        in {
          packages = forAll (system: { default = mkEnv system; });
        };
    }
    """
  end

  defp last_lines(out, n) do
    out
    |> String.split("\n")
    |> Enum.reject(&(&1 == ""))
    |> Enum.take(-n)
    |> Enum.join("\n")
    |> String.slice(0, 4000)
  end

  @dialyzer {:nowarn_function, stringify: 1}
  defp stringify(reason) do
    if is_binary(reason) do
      String.slice(reason, 0, 8000)
    else
      reason |> inspect(limit: 40) |> String.slice(0, 8000)
    end
  end

  # Builds interrupted by restart leave rows stuck at "building"; fail them
  # so clients see a terminal state and can re-request.
  defp sweep_orphans do
    import Ecto.Query

    from(e in Environment, where: e.status == "building")
    |> Repo.update_all(set: [status: "failed", error: "build interrupted by restart"])

    :ok
  end
end
