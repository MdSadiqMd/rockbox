defmodule RockboxWeb.Router do
  use Phoenix.Router

  pipeline :api do
    plug(:accepts, ["json"])
    plug(RockboxWeb.Plugs.Authenticate)
    plug(RockboxWeb.Plugs.RateLimit)
  end

  pipeline :api_json do
    plug(:accepts, ["json"])
  end

  pipeline :admin_auth do
    plug(RockboxWeb.Plugs.AdminAuthenticate)
  end

  scope "/api", RockboxWeb do
    pipe_through(:api)

    post("/execute", ExecuteController, :create)
    post("/sessions", SessionController, :create)
    post("/sessions/:id/execute", SessionController, :execute)
    delete("/sessions/:id", SessionController, :delete)
    get("/sessions/:id/files", FileController, :show)
    get("/sessions/:id/files/content", FileController, :content)
    put("/sessions/:id/files", FileController, :write)
    delete("/sessions/:id/files", FileController, :delete)

    get("/vms", VMController, :index)
    get("/vms/:id", VMController, :show)
    post("/vms", VMController, :create)
    delete("/vms/:id", VMController, :delete)
    post("/vms/:id/execute", VMController, :execute)

    post("/rl/episodes", RLController, :create)
    post("/rl/episodes/:episode_id/step", RLController, :step)
    post("/rl/episodes/:episode_id/steps", RLController, :steps)
    post("/rl/episodes/:episode_id/pause", RLController, :pause)
    delete("/rl/episodes/:episode_id", RLController, :delete)
    get("/rl/episodes/:episode_id/files", FileController, :show)
    get("/rl/episodes/:episode_id/files/content", FileController, :content)
    put("/rl/episodes/:episode_id/files", FileController, :write)
    delete("/rl/episodes/:episode_id/files", FileController, :delete)

    get("/usage", UsageController, :show)

    get("/environments", EnvironmentController, :index)
    post("/environments", EnvironmentController, :create)
    get("/environments/:id", EnvironmentController, :show)
    delete("/environments/:id", EnvironmentController, :delete)
  end

  scope "/api/admin", RockboxWeb do
    pipe_through([:api_json, :admin_auth])

    get("/workspaces", AdminController, :show_workspaces)
    post("/workspaces", AdminController, :create_workspace)

    post("/workspaces/:workspace_id/keys", AdminController, :create_key)
    get("/workspaces/:workspace_id/keys", AdminController, :list_keys)
    delete("/workspaces/:workspace_id/keys/:key_id", AdminController, :revoke_key)
  end

  scope "/" do
    pipe_through([])

    get("/health", RockboxWeb.HealthController, :show)
    get("/ready", RockboxWeb.HealthController, :ready)
  end
end
