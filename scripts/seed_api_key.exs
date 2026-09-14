alias Rockbox.{Repo, Workspaces, ApiKeys}
import Ecto.Query

{:ok, _} = Application.ensure_all_started(:ecto_sql)
{:ok, _} = Repo.start_link(pool_size: 2)

Repo.delete_all(from(k in ApiKeys.ApiKey, where: k.workspace_id == "ws_pro_demo"))
Repo.delete_all(from(w in Workspaces.Workspace, where: w.id == "ws_pro_demo" or w.name == "demo-pro"))

{:ok, _} =
  Workspaces.create_workspace(%{id: "ws_pro_demo", name: "demo-pro", tier: "pro", concurrent_max: 200})

{:ok, %{raw: raw, key: _}} = ApiKeys.generate("ws_pro_demo", name: "bench-key")
IO.puts("KEY:" <> raw)
