defmodule RockboxWeb.Plugs.Authenticate do
  @moduledoc """
  Bearer-token auth. In dev/test we accept `token-<workspace>-<tier>` for
  simple scripted clients. Production should swap in JWT verification via
  Joken + the workspace registry
  """

  import Plug.Conn

  def init(opts), do: opts

  def call(conn, _opts) do
    with [bearer] <- get_req_header(conn, "authorization"),
         {:ok, %{workspace_id: wid, tier: tier} = ctx} <- decode(bearer) do
      conn
      |> assign(:caller_workspace, wid)
      |> assign(:caller_tier, tier)
      |> assign(:caller, ctx)
    else
      _ ->
        conn
        |> put_resp_content_type("application/json")
        |> send_resp(401, Jason.encode!(%{error: "unauthorized"}))
        |> halt()
    end
  end

  defp decode("Bearer " <> rest), do: decode(rest)

  defp decode("token-" <> rest) do
    case String.split(rest, "-", parts: 2) do
      [workspace_id, tier_str] ->
        {:ok, %{workspace_id: workspace_id, tier: String.to_atom(tier_str), user_id: nil}}

      _ ->
        :error
    end
  end

  defp decode(_), do: :error
end
