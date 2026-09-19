defmodule Rockbox.EnvironmentsTest do
  use ExUnit.Case, async: false

  alias Rockbox.{Environments, Repo}
  import Ecto.Query

  @ws "test-ws-env-#{System.unique_integer([:positive])}"

  setup_all do
    import Ecto.Query

    from(w in "workspaces", where: w.id == ^@ws) |> Repo.delete_all()
    {:ok, _} = Rockbox.Workspaces.create_workspace(%{id: @ws, name: "envs-#{@ws}", tier: "pro"})

    on_exit(fn ->
      import Ecto.Query
      from(e in "custom_environments", where: e.workspace_id == ^@ws) |> Repo.delete_all()
      from(w in "workspaces", where: w.id == ^@ws) |> Repo.delete_all()
    end)

    %{ws: @ws}
  end

  describe "create/2 validation" do
    test "rejects unsupported languages", %{ws: ws} do
      assert {:error, %{language: "unsupported"}} =
               Environments.create(ws, %{
                 "language" => "fortran",
                 "spec" => %{"kind" => "flake", "flake_nix" => "outputs"}
               })
    end

    test "rejects bad package names", %{ws: ws} do
      assert {:error, %{spec: _}} =
               Environments.create(ws, %{
                 "language" => "python",
                 "spec" => %{"kind" => "python_packages", "packages" => ["numpy; rm -rf /"]}
               })
    end

    test "rejects empty and oversized package lists", %{ws: ws} do
      assert {:error, %{spec: _}} =
               Environments.create(ws, %{
                 "language" => "python",
                 "spec" => %{"kind" => "python_packages", "packages" => []}
               })

      big = List.duplicate("numpy", 65)

      assert {:error, %{spec: _}} =
               Environments.create(ws, %{
                 "language" => "python",
                 "spec" => %{"kind" => "python_packages", "packages" => big}
               })
    end

    test "rejects oversized flakes", %{ws: ws} do
      assert {:error, %{spec: _}} =
               Environments.create(ws, %{
                 "language" => "python",
                 "spec" => %{
                   "kind" => "flake",
                   "flake_nix" => String.duplicate("a", 40_000) <> " outputs"
                 }
               })
    end
  end

  describe "authorize_runtime/2" do
    test "passes through non-custom runtimes without DB work", %{ws: ws} do
      assert :ok = Environments.authorize_runtime(ws, nil)
      assert :ok = Environments.authorize_runtime(ws, "python-ml")
      assert :ok = Environments.authorize_runtime(ws, "ts-bun")
    end

    test "rejects unknown custom ids", %{ws: ws} do
      id = "11111111-2222-3333-4444-555555555555"
      assert {:error, [%{path: "runtime"}]} = Environments.authorize_runtime(ws, "custom-" <> id)
    end

    test "enforces workspace ownership", %{ws: ws} do
      {:ok, env} =
        Environments.create(ws, %{
          "language" => "python",
          "spec" => %{"kind" => "python_packages", "packages" => ["requests"]}
        })

      other = "test-ws-other-#{System.unique_integer([:positive])}"

      assert {:error, _} =
               Environments.authorize_runtime(other, Environments.runtime_name(env.id))
    after
      import Ecto.Query
      from(e in "custom_environments", where: e.workspace_id == ^@ws) |> Repo.delete_all()
    end
  end
end
