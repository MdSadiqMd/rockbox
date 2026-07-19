defmodule Rockbox.AuditLog do
  @moduledoc """
  Append-only audit trail. Every request writes - requested_settings + effective_settings + clamped[] + outcome

  Persisted to Postgres for 90 days by default. Drives billing reconciliation
  and security review

  We funnel writes through a single GenServer so the foreground request path
  never blocks on Postgres - writes are buffered and flushed in batches every
  1s or 64 entries (whichever first)
  """

  use GenServer
  require Logger
  alias Rockbox.Repo
  import Ecto.Query

  @flush_every_ms 1_000
  @flush_size 64

  defmodule Entry do
    use Ecto.Schema

    @primary_key {:id, :binary_id, autogenerate: true}
    @foreign_key_type :binary_id

    schema "audit_log" do
      field(:request_id, :string)
      field(:workspace_id, :string)
      field(:user_id, :string)
      field(:mode, :string)
      field(:language, :string)
      field(:runtime, :string)
      field(:settings_requested, :map)
      field(:settings_effective, :map)
      field(:clamped, {:array, :map})
      field(:status, :string)
      field(:exit_code, :integer)
      field(:exec_time_ms, :integer)
      field(:memory_peak_mb, :integer)
      field(:output_truncated, :boolean)
      field(:credits_spent, :integer)
      field(:error_summary, :string)

      timestamps(type: :utc_datetime_usec, updated_at: false)
    end
  end

  def start_link(_), do: GenServer.start_link(__MODULE__, %{}, name: __MODULE__)

  @doc """
  Record an audit entry. Returns immediately (`cast`); the row is flushed
  to Postgres within the next batch.
  """
  def record(%{} = entry), do: GenServer.cast(__MODULE__, {:record, entry})

  @impl true
  def init(_) do
    schedule_flush()
    {:ok, %{pending: []}}
  end

  @impl true
  def handle_cast({:record, entry}, %{pending: pending} = state) do
    new = [entry | pending]

    if length(new) >= @flush_size do
      flush(new)
      {:noreply, %{state | pending: []}}
    else
      {:noreply, %{state | pending: new}}
    end
  end

  @impl true
  def handle_info(:flush_tick, state) do
    schedule_flush()
    flush(state.pending)
    {:noreply, %{state | pending: []}}
  end

  defp schedule_flush, do: Process.send_after(self(), :flush_tick, @flush_every_ms)

  defp flush([]), do: :ok

  defp flush(entries) do
    rows = Enum.map(entries, &row/1)

    case Repo.insert_all(Entry, rows, on_conflict: :nothing) do
      {_n, _} -> :ok
    end
  rescue
    e ->
      Logger.error("audit_log flush failed: #{inspect(e)}")
      :ok
  end

  defp row(entry) do
    now = DateTime.utc_now()

    %{
      request_id: entry[:request_id],
      workspace_id: entry[:workspace_id],
      user_id: entry[:user_id],
      mode: stringify(entry[:mode]),
      language: stringify(entry[:language]),
      runtime: entry[:runtime],
      settings_requested: entry[:settings_requested] || %{},
      settings_effective: entry[:settings_effective] || %{},
      clamped: entry[:clamped] || [],
      status: stringify(entry[:status]),
      exit_code: entry[:exit_code],
      exec_time_ms: entry[:exec_time_ms],
      memory_peak_mb: entry[:memory_peak_mb],
      output_truncated: entry[:output_truncated] || false,
      credits_spent: entry[:credits_spent] || 0,
      error_summary: entry[:error_summary],
      inserted_at: now
    }
  end

  defp stringify(nil), do: nil
  defp stringify(atom) when is_atom(atom), do: Atom.to_string(atom)
  defp stringify(other), do: to_string(other)

  @doc "Convenience for tests + admin tools."
  def list_recent(limit \\ 50) do
    Repo.all(from(e in Entry, order_by: [desc: e.inserted_at], limit: ^limit))
  end
end
