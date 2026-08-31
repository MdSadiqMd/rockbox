defmodule Rockbox.Wire do
  @moduledoc """
  Msgpack wire protocol shared with the Rust sandbox engine

  Mirrors `core/crates/protocol`. Keep field names + tag values in
  lockstep — adding/renaming a field is a synchronized change on both sides

  Frames are wrapped with `Port.open(..., [{:packet, 4}, :binary])`, which
  takes care of the 4-byte big-endian length prefix. This module is concerned
  only with the msgpack payload
  """

  @schema_version "v1"
  def schema_version, do: @schema_version

  @doc "Encode a command to wire bytes (msgpack)."
  @spec encode_command(map()) :: iodata()
  def encode_command(%{} = cmd), do: Msgpax.pack!(cmd, iodata: true)

  @doc "Construct an `execute` command around a frozen settings map."
  def execute(settings) when is_map(settings) do
    %{"cmd" => "execute", "schema" => @schema_version}
    |> Map.merge(stringify(settings))
    |> wrap_execute()
  end

  defp wrap_execute(map) do
    # Internally-tagged enum on the Rust side carries the entire Settings
    # as the variant payload. We embed it under `settings` for clarity in
    # logs and for symmetric Box<Settings> deserialization.
    %{"cmd" => "execute", "Execute" => Map.delete(map, "cmd")}
  end

  def exec_cell(id, session_id, code, opts \\ []) do
    %{
      "cmd" => "exec_cell",
      "id" => id,
      "session_id" => session_id,
      "code" => code,
      "files" => Keyword.get(opts, :files, []),
      "stdin" => Keyword.get(opts, :stdin),
      "wall_ms" => Keyword.get(opts, :wall_ms)
    }
  end

  # Action bytes travel as msgpack *bin*, not *str* — raw frames are not
  # necessarily valid UTF-8 and the engine's serde_bytes decoder expects a
  # byte sequence.
  def rl_step(id, episode_id, action) when is_binary(action) do
    %{
      "cmd" => "rl_step",
      "id" => id,
      "episode_id" => episode_id,
      "action" => Msgpax.Bin.new(action)
    }
  end

  @doc """
  Encode an EnvPool-style batched RL step command. `actions` is a list of
  binary action frames; every tick comes back in one `rl_steps` response.
  """
  def rl_steps(id, episode_id, actions) when is_list(actions) do
    %{
      "cmd" => "rl_steps",
      "id" => id,
      "episode_id" => episode_id,
      "actions" => Enum.map(actions, &Msgpax.Bin.new/1)
    }
  end

  def stdin(data) when is_binary(data), do: %{"cmd" => "stdin", "data" => data}
  def interrupt(id), do: %{"cmd" => "interrupt", "id" => id}
  def shutdown, do: %{"cmd" => "shutdown"}

  @doc """
  Decode a single msgpack frame coming back from the engine. Returns
  `{:ok, decoded_map}` or `{:error, reason}`.
  """
  @spec decode(binary()) :: {:ok, map()} | {:error, term()}
  def decode(<<payload::binary>>) do
    case Msgpax.unpack(payload) do
      {:ok, map} when is_map(map) -> {:ok, map}
      {:ok, other} -> {:error, {:expected_map, other}}
      {:error, reason} -> {:error, reason}
    end
  end

  @doc """
  Classify a decoded response by its `"type"` tag. Use `case classify(map)`
  in GenServer `handle_info` for clean dispatch.
  """
  @spec classify(map()) :: atom()
  def classify(%{"type" => t}) when is_binary(t), do: String.to_atom(t)
  def classify(_), do: :unknown

  @doc "Deeply convert atom keys to strings (Msgpax handles atoms but Rust expects strings)."
  @spec stringify(any()) :: any()
  def stringify(%_{} = struct), do: stringify(Map.from_struct(struct))
  def stringify(%{} = map), do: Map.new(map, fn {k, v} -> {to_string(k), stringify(v)} end)
  def stringify(list) when is_list(list), do: Enum.map(list, &stringify/1)

  def stringify(atom) when is_atom(atom) and atom not in [nil, true, false],
    do: Atom.to_string(atom)

  def stringify(other), do: other
end
