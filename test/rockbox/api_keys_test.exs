defmodule Rockbox.ApiKeysTest do
  use ExUnit.Case, async: false
  import Plug.Test
  import Plug.Conn

  alias Rockbox.{ApiKeys, Repo, Workspaces}

  setup do
    ws_id = "test-ws-#{System.unique_integer([:positive])}"

    {:ok, _ws} =
      Workspaces.create_workspace(%{id: ws_id, name: "auth-test-#{ws_id}", tier: "pro"})

    on_exit(fn -> cleanup(ws_id) end)

    %{workspace_id: ws_id}
  end

  defp cleanup(ws_id) do
    import Ecto.Query

    from(k in "api_keys", where: k.workspace_id == ^ws_id) |> Repo.delete_all()
    from(w in "workspaces", where: w.id == ^ws_id) |> Repo.delete_all()
    :ok
  end

  describe "generate/2" do
    test "returns raw key once and stores metadata only", %{workspace_id: ws} do
      {:ok, %{raw: raw, key: key}} = ApiKeys.generate(ws, name: "ci")

      assert String.starts_with?(raw, "rb_")
      assert byte_size(raw) == 46
      assert key.prefix == binary_slice(raw, 0, 10)
      refute is_nil(key.key_hash)
      assert key.name == "ci"
    end

    test "raw keys are unique", %{workspace_id: ws} do
      {:ok, %{raw: r1}} = ApiKeys.generate(ws)
      {:ok, %{raw: r2}} = ApiKeys.generate(ws)
      assert r1 != r2
    end
  end

  describe "verify/1" do
    test "accepts a freshly minted key and resolves the workspace tier", %{workspace_id: ws} do
      {:ok, %{raw: raw}} = ApiKeys.generate(ws)

      assert {:ok, %{workspace_id: ^ws, tier: "pro"}} = ApiKeys.verify(raw)
    end

    test "rejects unknown and malformed credentials" do
      assert :error =
               ApiKeys.verify(
                 "rb_" <> Base.url_encode64(:crypto.strong_rand_bytes(32), padding: false)
               )

      assert :error = ApiKeys.verify("token-free-demo")
      assert :error = ApiKeys.verify("")
    end

    test "rejects revoked keys immediately (cache purged)", %{workspace_id: ws} do
      {:ok, %{raw: raw, key: key}} = ApiKeys.generate(ws)
      assert {:ok, _} = ApiKeys.verify(raw)

      assert :ok = ApiKeys.revoke(ws, key.id)
      assert :error = ApiKeys.verify(raw)
    end

    test "expired cache entries fall through to the DB tombstone", %{workspace_id: ws} do
      {:ok, %{raw: raw}} = ApiKeys.generate(ws)
      assert {:ok, _} = ApiKeys.verify(raw)

      # revoke behind the cache's back, then force-expire the cached entry:
      # the DB lookup must still refuse the key
      hash = :crypto.hash(:sha256, raw)
      import Ecto.Query

      from(k in "api_keys", where: k.key_hash == ^hash)
      |> Repo.update_all(set: [revoked_at: DateTime.utc_now()])

      expire_entry(hash)

      assert :error = ApiKeys.verify(raw)
    end
  end

  describe "plug" do
    test "accepts rb_ keys and assigns workspace context", %{workspace_id: ws} do
      {:ok, %{raw: raw}} = ApiKeys.generate(ws)

      conn =
        conn(:get, "/")
        |> put_req_header("authorization", "Bearer #{raw}")
        |> RockboxWeb.Plugs.Authenticate.call([])

      refute conn.halted
      assert conn.assigns.caller_workspace == ws
      assert conn.assigns.caller_tier == :pro
    end

    test "rejects bad credentials with 401 + halt" do
      conn =
        conn(:get, "/")
        |> put_req_header("authorization", "Bearer rb_nope")
        |> RockboxWeb.Plugs.Authenticate.call([])

      assert conn.halted
      assert conn.status == 401
    end

    test "dev tokens honor allow_dev_tokens=false" do
      Application.put_env(:rockbox, :allow_dev_tokens, false)

      conn =
        conn(:get, "/")
        |> put_req_header("authorization", "Bearer token-somews-pro")
        |> RockboxWeb.Plugs.Authenticate.call([])

      assert conn.halted
      assert conn.status == 401
    after
      Application.delete_env(:rockbox, :allow_dev_tokens)
    end

    test "dev tokens work when allowed" do
      Application.put_env(:rockbox, :allow_dev_tokens, true)

      conn =
        conn(:get, "/")
        |> put_req_header("authorization", "Bearer token-somews-pro")
        |> RockboxWeb.Plugs.Authenticate.call([])

      refute conn.halted
      assert conn.assigns.caller_workspace == "somews"
      assert conn.assigns.caller_tier == :pro
    after
      Application.delete_env(:rockbox, :allow_dev_tokens)
    end
  end

  defp expire_entry(hash) do
    now = System.system_time(:millisecond)
    true = :ets.insert(:rockbox_api_key_cache, {hash, %{key_id: "stale"}, now - 1})
    :ok
  end
end
