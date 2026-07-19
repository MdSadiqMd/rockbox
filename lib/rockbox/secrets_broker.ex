defmodule Rockbox.SecretsBroker do
  @moduledoc """
  Resolves `secrets: [{name, ref}]` entries from the caller into a concrete
  `%{name => value}` map. Backend is pluggable, production reads from Vault
  or AWS Secrets Manager; the default in-memory backend reads from
  `Application.get_env(:rockbox, :secrets, %{})` so dev/test work offline

  Returned values are never logged and are never echoed back in
  `settings_effective` (see [`RockboxWeb.ExecuteJSON`])
  """

  use GenServer
  require Logger

  @cache_ttl_ms 60_000

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc """
  Resolve a list of `%{"name" => k, "ref" => uri}` entries. Returns
  `{:ok, %{name => value}}` or `{:error, {:unresolved, [name]}}`.
  """
  @spec resolve([map()]) :: {:ok, %{String.t() => String.t()}} | {:error, term()}
  def resolve([]), do: {:ok, %{}}

  def resolve(list) when is_list(list) do
    GenServer.call(__MODULE__, {:resolve, list}, 5_000)
  end

  @impl true
  def init(_), do: {:ok, %{cache: %{}}}

  @impl true
  def handle_call({:resolve, list}, _from, state) do
    now = System.monotonic_time(:millisecond)

    {results, unresolved, new_cache} =
      Enum.reduce(list, {%{}, [], state.cache}, fn entry, {acc, miss, cache} ->
        name = entry["name"] || entry[:name]
        ref = entry["ref"] || entry[:ref]

        case lookup(cache, ref, now) do
          {:hit, value} ->
            {Map.put(acc, name, value), miss, cache}

          :miss ->
            case fetch(ref) do
              {:ok, value} ->
                {Map.put(acc, name, value), miss,
                 Map.put(cache, ref, {value, now + @cache_ttl_ms})}

              :error ->
                {acc, [name | miss], cache}
            end
        end
      end)

    reply =
      case unresolved do
        [] -> {:ok, results}
        names -> {:error, {:unresolved, names}}
      end

    {:reply, reply, %{state | cache: new_cache}}
  end

  defp lookup(cache, ref, now) do
    case Map.get(cache, ref) do
      {value, expires_at} when expires_at > now -> {:hit, value}
      _ -> :miss
    end
  end

  defp fetch("vault://" <> path) do
    case Application.get_env(:rockbox, :secrets, %{}) do
      %{^path => value} -> {:ok, value}
      _ -> :error
    end
  end

  defp fetch("env://" <> name) do
    case System.get_env(name) do
      nil -> :error
      v -> {:ok, v}
    end
  end

  defp fetch(other) do
    Logger.warning("secrets_broker: unsupported ref scheme: #{inspect(other)}")
    :error
  end
end
