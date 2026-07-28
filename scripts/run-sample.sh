#!/usr/bin/env bash
# scripts/run-sample.sh — POST any sample to /api/execute
#
# Usage:
#   scripts/run-sample.sh <path>                 # single file, auto-detect lang
#   scripts/run-sample.sh <dir>  python main.py  # multi-file dir + entrypoint
#   ROCKBOX_URL=...   scripts/run-sample.sh ...  # override target
#   ROCKBOX_TOKEN=... scripts/run-sample.sh ...
#   PRETTY=1          scripts/run-sample.sh ...  # pipe through jq

set -euo pipefail

URL="${ROCKBOX_URL:-http://localhost:4000}"
TOKEN="${ROCKBOX_TOKEN:-token-ws_pro_demo-pro}"

if (( $# < 1 )); then
  cat <<EOF >&2
Usage: $0 <file-or-dir> [language] [entrypoint]
Examples:
  $0 priv/samples/python/hello.py
  $0 priv/samples/go/goroutines.go
  $0 priv/samples/multi_file python main.py
EOF
  exit 64
fi

target=$1
shift

if [[ -d $target ]]; then
  lang=${1:-python}
  entry=${2:-main.${lang}}
  runtime=$(default_runtime_for "$lang" 2>/dev/null || echo "")
  if [[ -z $runtime ]]; then
    case $lang in
      python) runtime=python-base ;;
      typescript|ts) lang=typescript; runtime=ts-modern ;;
      go) runtime=go-std ;;
      rust) runtime=rust-tokio ;;
      cpp) runtime=cpp-modern ;;
      *) echo "unknown language: $lang" >&2; exit 65 ;;
    esac
  fi

  files_json="["
  first=1
  while IFS= read -r -d '' f; do
    rel=${f#"$target"/}
    content=$(jq -Rs . <"$f")
    if (( first )); then first=0; else files_json+=,; fi
    files_json+=$(printf '{"path":%s,"content":%s}' "$(jq -n --arg p "$rel" '$p')" "$content")
  done < <(find "$target" -type f -print0)
  files_json+="]"
else
  ext=${target##*.}
  case $ext in
    py)  lang=python;     runtime=python-base; entry=$(basename "$target") ;;
    ts)  lang=typescript; runtime=ts-modern;   entry=$(basename "$target") ;;
    go)  lang=go;         runtime=go-std;      entry=$(basename "$target") ;;
    rs)  lang=rust;       runtime=rust-tokio;  entry=$(basename "$target") ;;
    cpp) lang=cpp;        runtime=cpp-modern;  entry=$(basename "$target") ;;
    *)   echo "unknown extension: .$ext (use a dir + language arg instead)" >&2; exit 65 ;;
  esac
  content=$(jq -Rs . <"$target")
  files_json=$(printf '[{"path":%s,"content":%s}]' "$(jq -n --arg p "$entry" '$p')" "$content")
fi

# Compiled languages run rustc/g++/go-build in the engine before sandbox
# launch. The wall_ms cap only covers the sandbox exec (the binary run),
# not the compile step — so interpreted and compiled languages need the
# same wall_ms for execution. We still give compiled langs a bit more
# headroom (10 s) since their binaries tend to do heavier startup work.
# Go uses a persistent GOCACHE so post-warmup compile is < 0.5 s.
case $lang in
  rust|cpp)  wall_ms=${WALL_MS:-10000} ;;
  go)        wall_ms=${WALL_MS:-10000} ;;
  *)         wall_ms=${WALL_MS:-5000}  ;;
esac

payload=$(jq -n \
  --arg lang "$lang" \
  --arg rt "$runtime" \
  --arg entry "$entry" \
  --argjson files "$files_json" \
  --argjson wall_ms "$wall_ms" \
  '{settings: {language: $lang, runtime: $rt, entrypoint: $entry,
               files: $files, limits: {wall_ms: $wall_ms}}}')

echo "→ POST $URL/api/execute   language=$lang runtime=$runtime entrypoint=$entry" >&2

if [[ ${PRETTY:-} == 1 ]]; then
  curl -sS -X POST "$URL/api/execute" \
    -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    -d "$payload" | jq
else
  curl -sS -X POST "$URL/api/execute" \
    -H "authorization: Bearer $TOKEN" \
    -H 'content-type: application/json' \
    -d "$payload"
  echo
fi
