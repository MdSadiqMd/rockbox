defmodule Rockbox.ApiKeys do
  @moduledoc """
  API keys for workspace authentication.

  A raw key (`rb_<43 url-safe chars>`) is shown exactly once at creation; only
  its SHA-256 digest is stored. Verification runs on an ETS fast path
  (~sub-microsecond, read_concurrency) backed by a Postgres lookup that also
  resolves the workspace tier; cached entries live 60 s so a revocation
  propagates within one TTL even without invalidation — revocation additionally
  purges the cache entry immediately.

  The DB path doubles as the throttled `last_used_at` toucher: it runs at
  most once per TTL window per active key instead of once per request.
  """

  import Ecto.Query
  alias Rockbox.ApiKeys.Cache
  alias Rockbox.Repo

  @cache_ttl_ms 60_000
  @key_prefix "rb_"

  def cache_ttl_ms, do: @cache_ttl_ms

  defmodule ApiKey do
    use Ecto.Schema

    @primary_key {:id, :binary_id, autogenerate: true}
    @foreign_key_type :binary_id

    schema "api_keys" do
      field(:workspace_id, :string)
      field(:name, :string, default: "default")
      field(:prefix, :string)
      field(:key_hash, :binary)
      field(:revoked_at, :utc_datetime_usec)
      field(:last_used_at, :utc_datetime_usec)

      timestamps(type: :utc_datetime_usec)
    end
  end

  @doc """
  Mints a key for `workspace`. Returns `{:ok, %{raw: raw, key: key}}` where
  `raw` is the only time the full key material exists anywhere.
  """
  def generate(workspace_id, attrs \\ []) do
    name = Keyword.get(attrs, :name, "default")
    raw = @key_prefix <> Base.url_encode64(:crypto.strong_rand_bytes(32), padding: false)
    prefix = binary_slice(raw, 0, 10)

    %ApiKey{}
    |> Ecto.Changeset.cast(%{workspace_id: workspace_id, name: name, prefix: prefix}, [
      :workspace_id,
      :name,
      :prefix
    ])
    |> Ecto.Changeset.put_change(:key_hash, hash(raw))
    |> Ecto.Changeset.validate_required([:workspace_id, :prefix, :key_hash])
    |> Repo.insert()
    |> case do
      {:ok, key} -> {:ok, %{raw: raw, key: key}}
      error -> error
    end
  end

  @doc "Verifies a raw bearer credential against issued, non-revoked keys. Returns `{:ok, ctx}` or `:error`."
  def verify(@key_prefix <> _ = raw) do
    h = hash(raw)

    case Cache.get(h) do
      {:ok, ctx} -> {:ok, ctx}
      :miss -> load_and_cache(h)
    end
  end

  def verify(_), do: :error

  defp load_and_cache(h) do
    query =
      from(k in ApiKey,
        join: w in Rockbox.Workspaces.Workspace,
        on: w.id == k.workspace_id,
        where: k.key_hash == ^h and is_nil(k.revoked_at),
        select: %{key_id: k.id, workspace_id: k.workspace_id, tier: w.tier},
        update: [set: [last_used_at: ^DateTime.utc_now()]]
      )

    case Repo.update_all(query, []) do
      {1, [ctx]} ->
        Cache.put(h, ctx)
        {:ok, ctx}

      _ ->
        :error
    end
  end

  @doc "Revokes a key immediately (DB tombstone + cache purge)."
  def revoke(workspace_id, key_id) do
    result =
      from(k in ApiKey, where: k.id == ^key_id and k.workspace_id == ^workspace_id)
      |> Repo.update_all(set: [revoked_at: DateTime.utc_now()])

    Cache.purge(key_id)

    case result do
      {1, _} -> :ok
      {0, _} -> {:error, :not_found}
    end
  end

  def list_keys(workspace_id) do
    from(k in ApiKey,
      where: k.workspace_id == ^workspace_id,
      select: %{
        id: k.id,
        name: k.name,
        prefix: k.prefix,
        created_at: k.inserted_at,
        last_used_at: k.last_used_at,
        revoked_at: k.revoked_at
      },
      order_by: [desc: k.inserted_at]
    )
    |> Repo.all()
  end

  def get_key(workspace_id, key_id),
    do: Repo.get_by(ApiKey, id: key_id, workspace_id: workspace_id)

  defp hash(raw), do: :crypto.hash(:sha256, raw)
end
