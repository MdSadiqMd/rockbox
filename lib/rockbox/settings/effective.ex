defmodule Rockbox.Settings.Effective do
  @moduledoc """
  Immutable, frozen settings produced by [`Rockbox.Settings.Pipeline.run/2`]

  Every value here is post-merge, post-clamp, post-secrets-resolution.
  Downstream code (VM GenServer, AuditLog, CostTracker, engine) reads only
  this struct — no further re-resolution allowed
  """

  @enforce_keys [
    :request_id,
    :workspace_id,
    :tier,
    :language,
    :runtime,
    :files,
    :entrypoint,
    :mode,
    :limits,
    :lifecycle,
    :capabilities,
    :network,
    :filesystem,
    :env,
    :resolved_secrets,
    :output,
    :observability,
    :gpu,
    :determinism,
    :cost,
    :labels,
    :session_id,
    :stdin,
    :clamped
  ]

  defstruct @enforce_keys ++ [strict: false]

  @typedoc """
  A frozen Settings struct — never mutated after `Pipeline.run/2` returns.
  Pass it by reference (Elixir's structural sharing keeps this cheap).
  """
  @type t :: %__MODULE__{
          request_id: String.t(),
          workspace_id: String.t(),
          tier: atom(),
          language: atom(),
          runtime: String.t(),
          files: [map()],
          entrypoint: String.t(),
          mode: atom(),
          limits: map(),
          lifecycle: map(),
          capabilities: [atom()],
          network: map(),
          filesystem: map(),
          env: %{String.t() => String.t()},
          resolved_secrets: %{String.t() => String.t()},
          output: map(),
          observability: map(),
          gpu: map(),
          determinism: map(),
          cost: map(),
          labels: %{String.t() => String.t()},
          session_id: String.t() | nil,
          stdin: binary() | nil,
          clamped: [map()],
          strict: boolean()
        }

  @doc """
  Build the msgpack-shaped map the Rust engine expects. Keys are strings,
  enum atoms are stringified, file `content` is sent as raw bytes.
  """
  def to_wire(%__MODULE__{} = s) do
    %{
      "schema" => Rockbox.Wire.schema_version(),
      "request_id" => s.request_id,
      "labels" => s.labels,
      "language" => Atom.to_string(s.language),
      "runtime" => s.runtime,
      "files" => Enum.map(s.files, &file_to_wire/1),
      "entrypoint" => s.entrypoint,
      "mode" => Atom.to_string(s.mode),
      "session_id" => s.session_id,
      "limits" => s.limits,
      "lifecycle" => s.lifecycle,
      "capabilities" => Enum.map(s.capabilities, &Atom.to_string/1),
      "network" => s.network,
      "filesystem" => s.filesystem,
      "env" => s.env,
      "resolved_secrets" => s.resolved_secrets,
      "stdin" => s.stdin,
      "determinism" => s.determinism,
      "gpu" => s.gpu,
      "output" => s.output,
      "observability" => s.observability,
      "cost" => s.cost
    }
  end

  defp file_to_wire(%{} = f) do
    %{
      "path" => f["path"] || f[:path],
      "content" => f["content"] || f[:content],
      "mode" => f["mode"] || f[:mode] || 0o644
    }
  end
end
