defmodule RockboxWeb.Plugs.RateLimit do
  @moduledoc """
  Per-workspace token-bucket rate limit on expensive API operations
  (VM-spawning creates, writes, deletes).

  Hot paths are deliberately exempt: RL `step`/`steps` run at thousands of
  calls/s per training worker and are bounded by the concurrency cap +
  `QuotaTracker.bump_steps/2` metering instead — bucketing them would throttle
  legitimate training loops.

  Returns 429 with a Retry-After hint when the bucket is empty. Bucket size /
  refill come from `rate_burst` / `rate_per_s` app env (env-tunable in
  runtime.exs).
  """

  import Plug.Conn

  @limited_methods ~w(POST PUT DELETE)

  def init(opts), do: opts

  def call(conn, _opts) do
    if conn.method in @limited_methods and not hot_path?(conn.path_info) do
      check(conn)
    else
      conn
    end
  end

  defp check(conn) do
    case Rockbox.QuotaTracker.take_token(conn.assigns.caller_workspace) do
      :ok ->
        conn

      {:error, :rate_limited} ->
        retry_after = retry_after_s()

        conn
        |> put_resp_header("retry-after", Integer.to_string(retry_after))
        |> put_resp_content_type("application/json")
        |> send_resp(429, Jason.encode!(%{error: "rate_limited", retry_after_s: retry_after}))
        |> halt()
    end
  end

  # Stepping is the high-frequency hot path: bounded by the concurrency cap
  # and metered via bump_steps, not the token bucket.
  defp hot_path?(["api", "rl", "episodes", _id, "step"]), do: true
  defp hot_path?(["api", "rl", "episodes", _id, "steps"]), do: true
  defp hot_path?(_), do: false

  defp retry_after_s do
    Application.get_env(:rockbox, :rate_retry_after_s, 1)
  end
end
