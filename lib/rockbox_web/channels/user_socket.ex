defmodule RockboxWeb.UserSocket do
  use Phoenix.Socket

  channel("vm:*", RockboxWeb.VMChannel)
  channel("session:*", RockboxWeb.SessionChannel)

  @impl true
  def connect(%{"token" => token}, socket, _connect_info) do
    case decode(token) do
      {:ok, ctx} -> {:ok, assign(socket, :caller, ctx)}
      _ -> :error
    end
  end

  def connect(_, _, _), do: :error

  @impl true
  def id(socket), do: "user:#{socket.assigns.caller.workspace_id}"

  defp decode("token-" <> rest) do
    case String.split(rest, "-", parts: 2) do
      [w, t] -> {:ok, %{workspace_id: w, tier: String.to_atom(t)}}
      _ -> :error
    end
  end

  defp decode(_), do: :error
end
