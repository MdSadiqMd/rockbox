defmodule RockboxWeb.Endpoint do
  use Phoenix.Endpoint, otp_app: :rockbox

  @session_options [
    store: :cookie,
    key: "_rockbox_key",
    signing_salt: "rockbox-sig",
    same_site: "Lax"
  ]

  socket("/socket", RockboxWeb.UserSocket,
    websocket: true,
    longpoll: false
  )

  if code_reloading? do
    plug(Phoenix.CodeReloader)
  end

  plug(Plug.RequestId)
  plug(Plug.Telemetry, event_prefix: [:phoenix, :endpoint])

  plug(Plug.Parsers,
    parsers: [:urlencoded, :multipart, :json],
    pass: ["*/*"],
    json_decoder: Phoenix.json_library(),
    length: 16_000_000
  )

  plug(Plug.MethodOverride)
  plug(Plug.Head)
  plug(Plug.Session, @session_options)

  plug(CORSPlug, origin: "*")

  plug(RockboxWeb.Router)
end
