defmodule RockboxWeb.Router do
  use Phoenix.Router

  pipeline :api do
    plug(:accepts, ["json"])
    plug(RockboxWeb.Plugs.Authenticate)
  end

  scope "/api", RockboxWeb do
    pipe_through(:api)

    post("/execute", ExecuteController, :create)
    post("/sessions", SessionController, :create)
    post("/sessions/:id/execute", SessionController, :execute)
    delete("/sessions/:id", SessionController, :delete)

    get("/vms", VMController, :index)
    get("/vms/:id", VMController, :show)
    post("/vms", VMController, :create)
    delete("/vms/:id", VMController, :delete)
    post("/vms/:id/execute", VMController, :execute)

    post("/rl/episodes", RLController, :create)
    post("/rl/episodes/:episode_id/step", RLController, :step)
    delete("/rl/episodes/:episode_id", RLController, :delete)
  end

  scope "/" do
    pipe_through([])

    get("/health", RockboxWeb.HealthController, :show)
    get("/ready", RockboxWeb.HealthController, :ready)
  end
end
