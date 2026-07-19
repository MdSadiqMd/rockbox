defmodule Rockbox.Settings.Validator do
  @moduledoc """
  Structural schema validation

  - Rejects unknown top-level keys (typo detection)
  - Coerces string enums to atoms for downstream pattern matching
  - Ensures required fields exist with the right shape
  - Leaves business-rule checks to [`Rockbox.Settings.CrossField`]
  """

  @top_keys ~w(
    schema request_id labels strict
    language runtime files entrypoint
    mode session_id
    limits lifecycle capabilities network filesystem env secrets stdin
    determinism gpu output observability cost
  )a

  @valid_languages ~w(python typescript go rust cpp)
  @valid_modes ~w(exec session rl_step rl_episode)

  @doc "Returns `{:ok, normalised}` or `{:error, [violation]}`."
  def validate(payload) when is_map(payload) do
    payload
    |> normalise_keys()
    |> check_unknown_keys()
    |> case do
      {:error, _} = err -> err
      {:ok, s} -> validate_required(s)
    end
  end

  def validate(_), do: {:error, [%{path: ".", reason: "settings must be a map"}]}

  # Step 1: atomise top-level keys + coerce enums
  defp normalise_keys(map) when is_map(map) do
    Enum.reduce(map, %{}, fn {k, v}, acc ->
      key = if is_binary(k), do: maybe_atom(k), else: k
      Map.put(acc, key, normalise_value(key, v))
    end)
  end

  defp maybe_atom(k)
       when k in ~w(schema request_id labels strict language runtime files entrypoint mode session_id
                                  limits lifecycle capabilities network filesystem env secrets stdin
                                  determinism gpu output observability cost) do
    String.to_atom(k)
  end

  defp maybe_atom(k), do: k

  defp normalise_value(:language, v) when is_binary(v), do: String.to_atom(v)
  defp normalise_value(:mode, v) when is_binary(v), do: String.to_atom(v)

  defp normalise_value(:capabilities, list) when is_list(list),
    do: Enum.map(list, &to_string/1)

  # Convert nested map keys to atoms so downstream lookups match @builtin atom keys.
  # Exclude env/labels (user-supplied arbitrary k/v) and capabilities (already coerced).
  defp normalise_value(key, v)
       when key in ~w(limits lifecycle network filesystem determinism gpu output observability cost)a and
              is_map(v),
       do: atomize_keys(v)

  defp normalise_value(_, v), do: v

  defp atomize_keys(map) when is_map(map),
    do: Map.new(map, fn {k, v} -> {maybe_atom_key(k), v} end)

  defp maybe_atom_key(k) when is_binary(k), do: String.to_atom(k)
  defp maybe_atom_key(k), do: k

  # Step 2: unknown-key check
  defp check_unknown_keys(map) do
    unknown =
      map
      |> Map.keys()
      |> Enum.reject(&(&1 in @top_keys))

    case unknown do
      [] -> {:ok, map}
      ks -> {:error, Enum.map(ks, &%{path: to_string(&1), reason: "unknown field"})}
    end
  end

  # Step 3: shape + required fields
  defp validate_required(s) do
    errs =
      []
      |> require_field(s, :language, "language is required")
      |> require_field(s, :files, "files[] is required and non-empty")
      |> validate_language(s)
      |> validate_mode(s)
      |> validate_files(s)

    if errs == [], do: {:ok, ensure_defaults(s)}, else: {:error, errs}
  end

  defp require_field(errs, s, key, reason) do
    case Map.fetch(s, key) do
      :error -> [%{path: to_string(key), reason: reason} | errs]
      {:ok, nil} -> [%{path: to_string(key), reason: reason} | errs]
      {:ok, []} when key == :files -> [%{path: "files", reason: reason} | errs]
      _ -> errs
    end
  end

  defp validate_language(errs, %{language: lang}) when is_atom(lang) do
    if Atom.to_string(lang) in @valid_languages do
      errs
    else
      [%{path: "language", reason: "must be one of #{Enum.join(@valid_languages, ", ")}"} | errs]
    end
  end

  defp validate_language(errs, _), do: errs

  defp validate_mode(errs, %{mode: mode}) when is_atom(mode) do
    if Atom.to_string(mode) in @valid_modes do
      errs
    else
      [%{path: "mode", reason: "must be one of #{Enum.join(@valid_modes, ", ")}"} | errs]
    end
  end

  # mode is optional — defaults to :exec in ensure_defaults.
  defp validate_mode(errs, _), do: errs

  defp validate_files(errs, %{files: list}) when is_list(list) do
    Enum.reduce(Enum.with_index(list), errs, fn {f, i}, acc ->
      cond do
        not is_map(f) ->
          [%{path: "files[#{i}]", reason: "must be a map"} | acc]

        not is_binary(f["path"] || f[:path]) ->
          [%{path: "files[#{i}].path", reason: "must be a string"} | acc]

        true ->
          acc
      end
    end)
  end

  defp validate_files(errs, _), do: errs

  defp ensure_defaults(%{} = s) do
    s
    |> Map.put_new(:mode, :exec)
    |> Map.put_new(:capabilities, [])
    |> Map.put_new(:strict, false)
    |> Map.put_new(:labels, %{})
  end
end
