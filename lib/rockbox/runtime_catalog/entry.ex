defmodule Rockbox.RuntimeCatalog.Entry do
  @moduledoc "One catalog entry for a named Nix runtime."

  @enforce_keys [:name, :language]
  defstruct [:name, :language, :baseline_caps, :baseline_env, :description]

  @type t :: %__MODULE__{
          name: String.t(),
          language: atom(),
          baseline_caps: [String.t()],
          baseline_env: %{String.t() => String.t()},
          description: String.t()
        }
end
