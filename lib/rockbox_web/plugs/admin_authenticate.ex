defmodule RockboxWeb.Plugs.AdminAuthenticate do
  @moduledoc """
  Guards `/api/admin/*`. Requires `config :rockbox, :admin_token` (set from
  ROCKBOX_ADMIN_TOKEN); requests must present it as the raw bearer credential.
  When no token is configured every admin request is refused — the surface
  simply does not exist until an operator provisions a token.
  """

  import Plug.Conn

  def init(opts), do: opts

  def call(conn, _opts) do
    expected = Application.get_env(:rockbox, :admin_token)

    with [_] <- if(expected in [nil, ""], do: [], else: [:configured]),
         [bearer] <- get_req_header(conn, "authorization"),
         "Bearer " <> presented <- bearer,
         true <- Plug.Crypto.secure_compare(presented, expected) do
      assign(conn, :admin, true)
    else
      _ ->
        conn
        |> put_resp_content_type("application/json")
        |> send_resp(403, Jason.encode!(%{error: "forbidden"}))
        |> halt()
    end
  end
end
