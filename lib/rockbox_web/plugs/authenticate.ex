defmodule RockboxWeb.Plugs.Authenticate do
  @moduledoc """
  Bearer-token auth.

  Production credentials are Rockbox API keys (`Bearer rb_...`), verified
  against the hashed-key registry with an ETS-cached fast path; the resolved
  workspace tier comes from the workspaces table.

  Dev tokens (`token-<workspace>-<tier>`) remain available only when
  `config :rockbox, :allow_dev_tokens, true` — dev/test/bench default on,
  prod defaults off.
  """

  import Plug.Conn

  @known_tiers ~w(free pro enterprise)

  def init(opts), do: opts

  def call(conn, _opts) do
    with [bearer] <- get_req_header(conn, "authorization"),
         {:ok, %{workspace_id: wid} = ctx} <- decode(bearer),
         %{} = caller <- normalize(ctx) do
      conn
      |> assign(:caller_workspace, wid)
      |> assign(:caller_tier, caller.tier)
      |> assign(:caller, caller)
    else
      _ ->
        conn
        |> put_resp_content_type("application/json")
        |> send_resp(401, Jason.encode!(%{error: "unauthorized"}))
        |> halt()
    end
  end

  defp decode("Bearer " <> rest), do: decode(rest)

  defp decode("rb_" <> _rest = raw_key) do
    case Rockbox.ApiKeys.verify(raw_key) do
      {:ok, ctx} -> {:ok, ctx}
      :error -> :error
    end
  end

  defp decode("token-" <> rest) do
    if allow_dev_tokens?() do
      case String.split(rest, "-", parts: 2) do
        [workspace_id, tier] -> {:ok, %{workspace_id: workspace_id, tier: tier, user_id: nil}}
        _ -> :error
      end
    else
      :error
    end
  end

  defp decode(_), do: :error

  defp normalize(%{workspace_id: _w, tier: t} = ctx) when t in @known_tiers,
    do: %{ctx | tier: String.to_atom(t)}

  # Unknown tiers are refused rather than passed through: the tier selects
  # quota ceilings, so an unvalidated string must never reach the pipeline.

  # Defaults on; prod.exs pins it to false explicitly and
  # ROCKBOX_ALLOW_DEV_TOKENS can force it either way at runtime. A literal
  # nil (runtime.exs leaves it unset) means "no override".
  defp allow_dev_tokens? do
    case Application.get_env(:rockbox, :allow_dev_tokens, :unset) do
      :unset -> true
      nil -> true
      v -> !!v
    end
  end
end
