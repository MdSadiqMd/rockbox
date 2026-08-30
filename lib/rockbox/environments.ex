defmodule Rockbox.Environments do
  @moduledoc """
  User-defined environments: workspace-owned Nix profiles built out-of-band
  and registered into every engine process on the host via JSON descriptors
  (see `core/crates/engine/src/runtime_catalog.rs` custom registry).

  A customer asks for `packages: ["numpy", "requests"]` (or uploads a raw
  flake); we generate + build the flake in a single-flight queue, pin the
  resulting store path, and hand back `runtime: "custom-<id>"`. Builds are
  content-addressed by nix itself — identical package sets across workspaces
  share store paths without any extra caching layer.
  """

  import Ecto.Query
  alias Ecto.Changeset
  alias Rockbox.Environments.Builder
  alias Rockbox.Repo

  defmodule Environment do
    use Ecto.Schema

    @primary_key {:id, :binary_id, autogenerate: true}
    @foreign_key_type :binary_id

    schema "custom_environments" do
      field(:workspace_id, :string)
      field(:name, :string)
      field(:language, :string)
      # User intent: %{"kind" => "python_packages", "packages" => [...]} or
      # %{"kind" => "flake", "flake_nix" => "<verbatim source>"}
      field(:spec, :map)
      field(:status, :string, default: "building")
      field(:store_path, :string)
      field(:bin_path, :string)
      field(:lock_hash, :binary)
      field(:error, :string)

      timestamps(type: :utc_datetime_usec)
    end
  end

  @languages ~w(python typescript go rust cpp)
  @max_packages 64
  @max_flake_bytes 32_768
  @package_re ~r/^[A-Za-z0-9_.][A-Za-z0-9_.-]{0,127}$/
  @slug_re ~r/^[a-z0-9][a-z0-9-]{0,48}$/

  @doc """
  Validates and registers an environment, then enqueues its build. Returns
  immediately with status "building"; poll GET /api/environments/:id.
  """
  def create(workspace_id, params) do
    language = params["language"]

    with {:ok, name} <- check_name(params["name"]),
         true <- language in @languages || {:error, %{language: "unsupported"}},
         {:ok, spec} <- validate_spec(params["spec"] || %{}, language) do
      changeset =
        %Environment{}
        |> Changeset.cast(
          %{
            workspace_id: workspace_id,
            name: name,
            language: language,
            spec: spec,
            status: "building"
          },
          [:workspace_id, :name, :language, :spec, :status]
        )
        |> Changeset.validate_required([:workspace_id, :name, :language, :spec])
        |> Changeset.unique_constraint([:workspace_id, :name])

      case Repo.insert(changeset) do
        {:ok, env} ->
          Builder.enqueue(env.id)
          {:ok, env}

        error ->
          error
      end
    else
      {:error, reason} -> {:error, reason}
      false -> {:error, %{language: "unsupported"}}
    end
  end

  def get(workspace_id, id), do: Repo.get_by(Environment, id: id, workspace_id: workspace_id)

  def list(workspace_id) do
    from(e in Environment,
      where: e.workspace_id == ^workspace_id,
      order_by: [desc: e.inserted_at]
    )
    |> Repo.all()
    |> Enum.map(&view/1)
  end

  def delete(workspace_id, id) do
    case get(workspace_id, id) do
      nil ->
        {:error, :not_found}

      env ->
        Builder.unregister(env)
        Repo.delete(env)
    end
  end

  @doc """
  Pipeline hook: `custom-<id>` runtimes must exist, be ready, and belong to
  the calling workspace. Non-custom names pass through untouched so the hot
  path pays nothing.
  """
  def authorize_runtime(_workspace_id, nil), do: :ok

  def authorize_runtime(workspace_id, name) when is_binary(name) do
    case Builder.parse_runtime_name(name) do
      nil ->
        :ok

      env_id ->
        case Repo.get(Environment, env_id) do
          %Environment{workspace_id: ^workspace_id, status: "ready"} -> :ok
          _ -> {:error, [%{path: "runtime", reason: "unknown environment #{name}"}]}
        end
    end
  end

  def authorize_runtime(_workspace_id, _), do: :ok

  @doc "The engine-visible runtime name for an environment id."
  defdelegate runtime_name(env_id), to: Rockbox.Environments.Builder

  def view(env) do
    %{
      id: env.id,
      name: env.name,
      language: env.language,
      status: env.status,
      runtime: Builder.runtime_name(env.id),
      error: env.error,
      inserted_at: env.inserted_at
    }
  end

  defp check_name(nil), do: {:ok, default_name()}

  defp check_name(n) when is_binary(n) do
    if Regex.match?(@slug_re, n), do: {:ok, n}, else: {:error, %{name: "must match [a-z0-9-]"}}
  end

  defp check_name(_), do: {:error, %{name: "invalid"}}

  defp default_name,
    do: "env-" <> Base.url_encode64(:crypto.strong_rand_bytes(6), padding: false)

  def validate_spec(%{"kind" => "python_packages", "packages" => pkgs}, "python") do
    if is_list(pkgs) and length(pkgs) in 1..@max_packages and
         Enum.all?(pkgs, &(is_binary(&1) and Regex.match?(@package_re, &1))) do
      {:ok, %{"kind" => "python_packages", "packages" => Enum.uniq(pkgs)}}
    else
      {:error,
       %{spec: "packages must be 1..#{@max_packages} strings matching #{@package_re.source}"}}
    end
  end

  def validate_spec(%{"kind" => "flake", "flake_nix" => src}, lang) when lang in @languages do
    if is_binary(src) and byte_size(src) <= @max_flake_bytes and String.contains?(src, "outputs") do
      {:ok, %{"kind" => "flake", "flake_nix" => src}}
    else
      {:error, %{spec: "flake_nix must be <=32KB and expose packages.default"}}
    end
  end

  def validate_spec(_, _lang), do: {:error, %{spec: "expected spec.kind=python_packages|flake"}}
end
