#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -gt 5 ]; then
  echo "usage: scripts/flamegraph.sh [raw|shaped] [sheet_name] [duration_secs] [concurrency] [requests_per_batch]" >&2
  exit 1
fi

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${TEST_SHEET_ID:?missing TEST_SHEET_ID in .env or environment}"

mode="${1:-shaped}"
sheet_name="${2:-${TEST_SHEET_NAME:-}}"
duration_secs="${3:-20}"
concurrency="${4:-64}"
requests_per_batch="${5:-1000}"
port="${PORT:-8080}"
output_dir="./flamegraphs"
viewer_dir="./bench-results/flamegraphs"
binary_output=""
viewer_output=""
server_pid=""
driver_pid=""
server_log="$(mktemp)"

cleanup() {
  if [ -n "${driver_pid}" ] && kill -0 "${driver_pid}" 2>/dev/null; then
    kill "${driver_pid}" 2>/dev/null || true
    wait "${driver_pid}" 2>/dev/null || true
  fi
  if [ -n "${server_pid}" ] && kill -0 "${server_pid}" 2>/dev/null; then
    kill "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
  fi
  rm -f "${server_log}"
}

trap cleanup EXIT

if [ -z "${sheet_name}" ]; then
  echo "missing sheet name: pass it as the second argument or set TEST_SHEET_NAME in .env" >&2
  exit 1
fi

if ! command -v cargo-flamegraph > /dev/null 2>&1 && ! cargo flamegraph --version > /dev/null 2>&1; then
  echo "cargo-flamegraph is not installed. Install it with: cargo install flamegraph" >&2
  exit 1
fi

if ! command -v perf > /dev/null 2>&1; then
  echo "perf is not installed or not present in PATH" >&2
  exit 1
fi

case "${mode}" in
  raw|shaped) ;;
  *)
    echo "mode must be 'raw' or 'shaped'" >&2
    exit 1
    ;;
esac

mkdir -p "${output_dir}"
mkdir -p "${viewer_dir}"

encoded_sheet_name="$(jq -nr --arg value "${sheet_name}" '$value | @uri')"
route="/${TEST_SHEET_ID}/${encoded_sheet_name}"
if [ "${mode}" = "raw" ]; then
  route="/raw${route}"
fi
url="http://127.0.0.1:${port}${route}"
timestamp_slug="$(date -u +%Y%m%dT%H%M%SZ)"
sheet_slug="$(printf '%s' "${sheet_name}" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9]/-/g; s/-\{2,\}/-/g; s/^-//; s/-$//')"
sheet_slug="${sheet_slug:-sheet}"
binary_output="${output_dir}/flamegraph-${mode}-${timestamp_slug}-${sheet_slug}.svg"
viewer_output="${viewer_dir}/$(basename "${binary_output}")"

echo "mode: ${mode}"
echo "sheet_name: ${sheet_name}"
echo "url: ${url}"
echo "duration_secs: ${duration_secs}"
echo "concurrency: ${concurrency}"
echo "requests_per_batch: ${requests_per_batch}"
echo "output: ${binary_output}"
echo

CARGO_PROFILE_RELEASE_DEBUG=true \
timeout --signal INT "${duration_secs}" \
  cargo flamegraph \
  --bin gsheet \
  --output "${binary_output}" \
  --deterministic \
  --palette rust \
  > "${server_log}" 2>&1 &
server_pid="$!"

for _ in $(seq 1 200); do
  if curl --silent --fail --output /dev/null "http://127.0.0.1:${port}/up"; then
    break
  fi
  if ! kill -0 "${server_pid}" 2>/dev/null; then
    break
  fi
  sleep 0.1
done

if ! kill -0 "${server_pid}" 2>/dev/null; then
  cat "${server_log}" >&2
  exit 1
fi

if ! curl --silent --fail --output /dev/null "http://127.0.0.1:${port}/up"; then
  echo "profiled server did not become ready on port ${port}" >&2
  cat "${server_log}" >&2
  exit 1
fi

curl --silent --fail --output /dev/null "${url}"

(
  while kill -0 "${server_pid}" 2>/dev/null; do
    seq "${requests_per_batch}" | xargs -P "${concurrency}" -I{} \
      sh -c "curl --silent --show-error --output /dev/null '${url}' >/dev/null 2>&1 || true"
  done
) &
driver_pid="$!"

wait "${server_pid}" || true

if [ ! -f "${binary_output}" ]; then
  cat "${server_log}" >&2
  exit 1
fi

cp "${binary_output}" "${viewer_output}"

index_path="${viewer_dir}/index.json"
timestamp_ms="$(date +%s%3N)"
tmp_index="$(mktemp)"

if [ -f "${index_path}" ]; then
  jq \
    --arg file "$(basename "${viewer_output}")" \
    --arg mode "${mode}" \
    --arg sheet_name "${sheet_name}" \
    --argjson timestamp "${timestamp_ms}" \
    '
    map(select(.file != $file))
    | [{file: $file, mode: $mode, sheet_name: $sheet_name, timestamp: $timestamp}] + .
    ' \
    "${index_path}" > "${tmp_index}"
else
  jq -n \
    --arg file "$(basename "${viewer_output}")" \
    --arg mode "${mode}" \
    --arg sheet_name "${sheet_name}" \
    --argjson timestamp "${timestamp_ms}" \
    '
    [{file: $file, mode: $mode, sheet_name: $sheet_name, timestamp: $timestamp}]
    ' > "${tmp_index}"
fi

mv "${tmp_index}" "${index_path}"

echo "flamegraph: ${binary_output}"

if command -v xdg-open > /dev/null 2>&1; then
  xdg-open "${binary_output}" > /dev/null 2>&1 || true
elif command -v open > /dev/null 2>&1; then
  open "${binary_output}" > /dev/null 2>&1 || true
fi
