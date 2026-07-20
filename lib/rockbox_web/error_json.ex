defmodule RockboxWeb.ErrorJSON do
  @moduledoc false

  def render("404.json", _), do: %{error: "not_found"}
  def render("500.json", _), do: %{error: "internal_server_error"}
  def render(template, _) when is_binary(template), do: %{error: template}

  def render(template, assigns) do
    %{
      error: Phoenix.Controller.status_message_from_template(template),
      details: Map.get(assigns, :reason)
    }
  end
end
