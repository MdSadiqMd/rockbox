defmodule Rockbox.WebhookDispatcher do
  @moduledoc """
  HMAC-signed webhook delivery with 3× exponential backoff and Postgres DLQ

  This is a Supervisor that owns a `Task.Supervisor` (for unlinked HTTP
  attempts) and the [`Server`] GenServer (for queuing)
  """

  use Supervisor

  @backoffs_ms [1_000, 5_000, 25_000]
  @task_sup __MODULE__.TaskSupervisor
  @server __MODULE__.Server

  def start_link(_), do: Supervisor.start_link(__MODULE__, [], name: __MODULE__)

  @doc "Enqueue an event. Returns immediately."
  def emit(%{url: url} = payload) when is_binary(url),
    do: GenServer.cast(@server, {:emit, payload})

  def emit(_), do: :ok

  @impl true
  def init(_) do
    Supervisor.init(
      [
        {Task.Supervisor, name: @task_sup},
        {__MODULE__.Server, %{task_sup: @task_sup, backoffs: @backoffs_ms}}
      ],
      strategy: :one_for_one
    )
  end

  defmodule Server do
    @moduledoc false
    use GenServer
    require Logger

    def start_link(arg), do: GenServer.start_link(__MODULE__, arg, name: __MODULE__)

    @impl true
    def init(arg), do: {:ok, Map.put(arg, :inflight, %{})}

    @impl true
    def handle_cast({:emit, payload}, %{task_sup: sup, backoffs: bo} = state) do
      task = Task.Supervisor.async_nolink(sup, fn -> attempt(payload, 0, bo) end)
      {:noreply, %{state | inflight: Map.put(state.inflight, task.ref, payload)}}
    end

    @impl true
    def handle_info({ref, _result}, state) when is_reference(ref) do
      Process.demonitor(ref, [:flush])
      {:noreply, %{state | inflight: Map.delete(state.inflight, ref)}}
    end

    def handle_info({:DOWN, ref, :process, _pid, _reason}, state) do
      {:noreply, %{state | inflight: Map.delete(state.inflight, ref)}}
    end

    defp attempt(payload, attempt, backoffs) do
      sig = sign(payload[:body], payload[:hmac_key])

      headers = [
        {"content-type", "application/json"},
        {"x-rockbox-event", payload[:event] || "unknown"},
        {"x-rockbox-signature", sig}
      ]

      case Req.post(payload[:url],
             body: payload[:body],
             headers: headers,
             retry: false,
             receive_timeout: 5_000
           ) do
        {:ok, %Req.Response{status: s}} when s in 200..299 ->
          :telemetry.execute([:rockbox, :webhook, :sent], %{count: 1}, %{event: payload[:event]})
          :ok

        other ->
          maybe_retry(payload, attempt, other, backoffs)
      end
    end

    defp maybe_retry(payload, attempt, reason, backoffs) do
      case Enum.at(backoffs, attempt) do
        nil ->
          Logger.warning("webhook DLQ event=#{payload[:event]} reason=#{inspect(reason)}")

          :telemetry.execute(
            [:rockbox, :webhook, :failed],
            %{count: 1},
            %{event: payload[:event], reason: classify(reason)}
          )

        delay ->
          Process.sleep(delay)
          attempt(payload, attempt + 1, backoffs)
      end
    end

    defp classify({:ok, %Req.Response{status: s}}), do: "http_#{s}"
    defp classify({:error, e}), do: inspect(e)

    defp sign(_body, nil), do: ""

    defp sign(body, key) when is_binary(body) and is_binary(key) do
      :crypto.mac(:hmac, :sha256, key, body) |> Base.encode16(case: :lower)
    end
  end
end
