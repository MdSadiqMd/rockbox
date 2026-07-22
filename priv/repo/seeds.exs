# Dev/test seed data — a couple of workspaces across tiers so the auth
# tokens in `RockboxWeb.Plugs.Authenticate` resolve to real records

alias Rockbox.Repo

now = DateTime.utc_now()

rows = [
  %{
    id: "ws_free_demo",
    name: "Free Demo",
    tier: "free",
    default_settings: %{},
    concurrent_max: 5,
    inserted_at: now,
    updated_at: now
  },
  %{
    id: "ws_pro_demo",
    name: "Pro Demo",
    tier: "pro",
    default_settings: %{},
    concurrent_max: 50,
    inserted_at: now,
    updated_at: now
  }
]

case Repo.insert_all("workspaces", rows, on_conflict: :nothing) do
  {n, _} -> IO.puts("seeded #{n} workspaces")
end
