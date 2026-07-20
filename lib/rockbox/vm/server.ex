defmodule Rockbox.VM.Server do
  @moduledoc """
  Per-VM GenServer. Wraps exactly one Rust `engine` OS process via
  an Erlang Port

  Lifecycle:
  1. `init/1` opens the Port, starts the SOCK_DGRAM data-channel listener,
     and awaits the engine's `ready` message
  2. Callers send commands via [`execute/2`] / [`exec_cell/2`] / [`stdin/2`]
  3. Responses arrive as `{port, {:data, _}}` messages, are msgpack-decoded
     and broadcast on `Phoenix.PubSub` under topic `"vm:<id>"`
  4. `terminate/2` closes the port and broadcasts `:vm_stopped`

  All settings come pre-frozen — this module never re-resolves. Only the
  fields it directly needs (language, mode, session_id, request_id) live in
  state; the rest is passed straight through to the engine
  """

  use GenServer
  require Logger

  alias Rockbox.{Wire, VM, AuditLog, WebhookDispatcher}
  alias Rockbox.Settings.Effective

  @type t :: %__MODULE__.State{}

  defmodule State do
    @moduledoc false
    @enforce_keys [
      :vm_id,
      :workspace_id,
      :language,
      :mode,
      :port,
      :data_sock,
      :status,
      :started_at
    ]
    defstruct [
      :vm_id,
      :workspace_id,
      :language,
      :mode,
      :session_id,
      :port,
      :data_sock,
      :status,
      :started_at,
      :owner,
      :runtime,
      :webhook,
      pending: %{}
    ]
  end

  def start_link(opts),
    do: GenServer.start_link(__MODULE__, opts, name: VM.Registry.via(opts.vm_id))

  @doc "Send `mode=exec` with the given frozen settings."
  def execute(vm_id, %Effective{} = s), do: call(vm_id, {:execute, s})

  @doc "Send an `exec_cell` command (session mode)."
  def exec_cell(vm_id, cell), do: call(vm_id, {:exec_cell, cell})

  @doc "Send an `rl_step` command (rl_step / rl_episode mode)."
  def rl_step(vm_id, episode_id, action) when is_binary(action),
    do:
      call(
        vm_id,
        {:rl_step, Rockbox.Wire.rl_step(new_request_id(), episode_id, action)}
      )

  defp new_request_id, do: "req_" <> (:crypto.strong_rand_bytes(8) |> Base.encode16(case: :lower))

  def stdin(vm_id, data), do: cast(vm_id, {:stdin, data})
  def interrupt(vm_id, id), do: cast(vm_id, {:interrupt, id})
  def shutdown(vm_id), do: cast(vm_id, :graceful_shutdown)

  def status(vm_id), do: call(vm_id, :status)

  defp call(vm_id, msg, timeout \\ 5_000) do
    case VM.Registry.whereis(vm_id) do
      {:ok, pid} ->
        try do
          GenServer.call(pid, msg, timeout)
        catch
          :exit, _ -> {:error, :vm_down}
        end

      :error ->
        {:error, :not_found}
    end
  end

  defp cast(vm_id, msg) do
    case VM.Registry.whereis(vm_id) do
      {:ok, pid} -> GenServer.cast(pid, msg)
      :error -> {:error, :not_found}
    end
  end

  @impl true
  def init(opts) do
    vm_id = Map.fetch!(opts, :vm_id)
    workspace_id = Map.fetch!(opts, :workspace_id)
    language = Map.get(opts, :language, :python)
    mode = Map.get(opts, :mode, :exec)
    session_id = Map.get(opts, :session_id)

    {data_sock, sock_path} = open_data_socket(vm_id)
    port_opts = port_opts(language, mode, sock_path)
    engine_bin = engine_binary()

    case File.exists?(engine_bin) do
      true ->
        port =
          Port.open({:spawn_executable, engine_bin}, [
            :binary,
            :exit_status,
            {:packet, 4},
            {:args, port_opts}
          ])

        Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{vm_id}", {:vm_started, vm_id})

        state = %State{
          vm_id: vm_id,
          workspace_id: workspace_id,
          language: language,
          mode: mode,
          session_id: session_id,
          port: port,
          data_sock: data_sock,
          status: :booting,
          started_at: System.system_time(:millisecond),
          owner: Map.get(opts, :owner),
          runtime: Map.get(opts, :runtime),
          webhook: Map.get(opts, :webhook)
        }

        {:ok, state}

      false ->
        Logger.error("engine binary missing: #{engine_bin}")
        {:stop, {:engine_binary_missing, engine_bin}}
    end
  end

  @impl true
  def handle_call({:execute, %Effective{} = s}, _from, state) do
    # Internally-tagged enum on the Rust side — settings fields live at the
    # same level as `cmd`, not nested under `Execute`.
    payload = Map.merge(%{"cmd" => "execute"}, Effective.to_wire(s))
    Port.command(state.port, Msgpax.pack!(payload, iodata: true))
    pending = Map.put(state.pending, s.request_id, {:execute, s})
    {:reply, :ok, %{state | status: :running, pending: pending}}
  end

  def handle_call({:exec_cell, %{} = cell}, _from, state) do
    Port.command(state.port, Msgpax.pack!(cell, iodata: true))
    {:reply, :ok, state}
  end

  def handle_call({:rl_step, %{} = cmd}, _from, state) do
    Port.command(state.port, Msgpax.pack!(cmd, iodata: true))
    {:reply, :ok, state}
  end

  def handle_call(:status, _from, state) do
    {:reply, public_status(state), state}
  end

  @impl true
  def handle_cast({:stdin, data}, state) do
    Port.command(state.port, Msgpax.pack!(Wire.stdin(data), iodata: true))
    {:noreply, state}
  end

  def handle_cast({:interrupt, id}, state) do
    Port.command(state.port, Msgpax.pack!(Wire.interrupt(id), iodata: true))
    {:noreply, state}
  end

  def handle_cast(:graceful_shutdown, state) do
    Port.command(state.port, Msgpax.pack!(Wire.shutdown(), iodata: true))
    {:noreply, %{state | status: :stopping}}
  end

  @impl true
  def handle_info({port, {:data, bytes}}, %{port: port} = state) do
    case Wire.decode(bytes) do
      {:ok, msg} ->
        {:noreply, handle_response(msg, state)}

      {:error, reason} ->
        Logger.warning("[vm #{state.vm_id}] decode error: #{inspect(reason)}")
        {:noreply, state}
    end
  end

  def handle_info({port, {:exit_status, status}}, %{port: port} = state) do
    Logger.warning("[vm #{state.vm_id}] engine exit_status=#{status}")
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:engine_died, status})
    {:stop, {:engine_exit, status}, state}
  end

  # Data channel — raw stdout/stderr chunks.
  def handle_info({:udp, sock, _ip, _port, payload}, %{data_sock: sock} = state) do
    case payload do
      <<1, len::32, bytes::binary-size(len), _::binary>> ->
        Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:stdout, bytes})

      <<2, len::32, bytes::binary-size(len), _::binary>> ->
        Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:stderr, bytes})

      _ ->
        :ok
    end

    :inet.setopts(sock, active: :once)
    {:noreply, state}
  end

  def handle_info(_other, state), do: {:noreply, state}

  @impl true
  def terminate(_reason, state) do
    if state.data_sock, do: :gen_udp.close(state.data_sock)
    :ok
  end

  defp handle_response(%{"type" => "ready"} = msg, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:ready, msg})
    %{state | status: :idle}
  end

  defp handle_response(%{"type" => "result"} = msg, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:result, msg})

    record_audit(state, msg)
    emit_webhook(state, "completed", msg)

    new_pending = Map.delete(state.pending, msg["request_id"])
    %{state | status: :idle, pending: new_pending}
  end

  defp handle_response(%{"type" => "cell_result"} = msg, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:cell_result, msg})
    emit_webhook(state, "cell_completed", msg)
    state
  end

  defp handle_response(%{"type" => "rl_step"} = msg, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:rl_step, msg})
    state
  end

  defp handle_response(%{"type" => "engine_died"} = msg, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:engine_died, msg})
    emit_webhook(state, "engine_died", msg)
    state
  end

  defp handle_response(%{"type" => "metrics"} = msg, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:metrics, msg})
    state
  end

  defp handle_response(other, state) do
    Phoenix.PubSub.broadcast(Rockbox.PubSub, "vm:#{state.vm_id}", {:other, other})
    state
  end

  defp engine_binary, do: Application.get_env(:rockbox, :engine)[:binary]

  defp data_sock_dir, do: Application.get_env(:rockbox, :engine)[:data_socket_dir]

  defp open_data_socket(vm_id) do
    File.mkdir_p!(data_sock_dir())
    path = Path.join(data_sock_dir(), "#{vm_id}.sock")
    _ = File.rm(path)
    {:ok, sock} = :gen_udp.open(0, [:binary, {:active, :once}, {:reuseaddr, true}])
    {sock, path}
  end

  defp port_opts(_language, _mode, sock_path) do
    [
      "--data-socket",
      sock_path,
      "--log",
      System.get_env("ROCKBOX_ENGINE_LOG") || "info"
    ]
  end

  defp public_status(s) do
    %{
      vm_id: s.vm_id,
      workspace_id: s.workspace_id,
      language: s.language,
      mode: s.mode,
      session_id: s.session_id,
      status: s.status,
      uptime_ms: System.system_time(:millisecond) - s.started_at,
      pending: map_size(s.pending)
    }
  end

  defp record_audit(state, msg) do
    AuditLog.record(%{
      request_id: msg["request_id"],
      workspace_id: state.workspace_id,
      mode: state.mode,
      language: state.language,
      runtime: state.runtime,
      status: msg["status"],
      exit_code: msg["exit_code"],
      exec_time_ms: msg["exec_time_ms"],
      memory_peak_mb: msg["memory_peak_mb"],
      output_truncated: msg["output_truncated"]
    })
  end

  defp emit_webhook(%{webhook: nil}, _ev, _msg), do: :ok

  defp emit_webhook(%{webhook: wh} = state, event, msg) do
    WebhookDispatcher.emit(%{
      url: wh.url,
      event: event,
      hmac_key: wh.hmac_key,
      body: Jason.encode!(%{event: event, vm_id: state.vm_id, data: msg})
    })
  end
end
