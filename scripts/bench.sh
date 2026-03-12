#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -gt 3 ]; then
  echo "usage: scripts/bench.sh [sheet_name] [requests_per_level] [concurrency_levels_csv]" >&2
  exit 1
fi

if [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi

: "${TEST_SHEET_ID:?missing TEST_SHEET_ID in .env or environment}"

sheet_name="${1:-${TEST_SHEET_NAME:-}}"
requests="${2:-500}"
levels_csv="${3:-1,4,16,32,64,128,256}"
port="${PORT:-8080}"
binary="./target/release/gsheet"
results_dir="./bench-results"
hyperfine_warmup_runs="${BENCH_HYPERFINE_WARMUP_RUNS:-10}"
hyperfine_runs="${BENCH_HYPERFINE_RUNS:-200}"
server_pid=""
sampler_pid=""
viewer_pid=""
rss_file="$(mktemp)"
rss_jsonl="$(mktemp)"
server_log="$(mktemp)"
viewer_log="$(mktemp)"
raw_hyperfine_json="$(mktemp)"
shaped_hyperfine_json="$(mktemp)"
load_entries_jsonl="$(mktemp)"

cleanup() {
  if [ -n "${sampler_pid}" ] && kill -0 "${sampler_pid}" 2>/dev/null; then
    kill "${sampler_pid}" 2>/dev/null || true
    wait "${sampler_pid}" 2>/dev/null || true
  fi
  if [ -n "${server_pid}" ] && kill -0 "${server_pid}" 2>/dev/null; then
    kill "${server_pid}" 2>/dev/null || true
    wait "${server_pid}" 2>/dev/null || true
  fi
  if [ -n "${viewer_pid}" ] && kill -0 "${viewer_pid}" 2>/dev/null; then
    kill "${viewer_pid}" 2>/dev/null || true
    wait "${viewer_pid}" 2>/dev/null || true
  fi
  rm -f \
    "${rss_file}" \
    "${rss_jsonl}" \
    "${server_log}" \
    "${viewer_log}" \
    "${raw_hyperfine_json}" \
    "${shaped_hyperfine_json}" \
    "${load_entries_jsonl}"
}

trap cleanup EXIT

if [ -z "${sheet_name}" ]; then
  echo "missing sheet name: pass it as the first argument or set TEST_SHEET_NAME in .env" >&2
  exit 1
fi

if [ ! -x "${binary}" ]; then
  echo "building release binary..."
  cargo build --release --bin gsheet
fi

mkdir -p "${results_dir}"

encoded_sheet_name="$(jq -nr --arg value "${sheet_name}" '$value | @uri')"
raw_url="http://127.0.0.1:${port}/raw/${TEST_SHEET_ID}/${encoded_sheet_name}"
shaped_url="http://127.0.0.1:${port}/${TEST_SHEET_ID}/${encoded_sheet_name}"
timestamp="$(date +%s000)"
timestamp_slug="$(date -u +%Y%m%dT%H%M%SZ)"
sheet_slug="$(printf '%s' "${sheet_name}" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9]/-/g; s/-\{2,\}/-/g; s/^-//; s/-$//')"
sheet_slug="${sheet_slug:-sheet}"
result_basename="run-${timestamp_slug}-${sheet_slug}.json"
result_file="${results_dir}/${result_basename}"

"${binary}" > "${server_log}" 2>&1 &
server_pid="$!"

for _ in $(seq 1 100); do
  if curl --silent --show-error --fail --output /dev/null "http://127.0.0.1:${port}/up"; then
    break
  fi
  sleep 0.1
done

if ! kill -0 "${server_pid}" 2>/dev/null; then
  cat "${server_log}" >&2
  exit 1
fi

if ! curl --silent --show-error --fail --output /dev/null "http://127.0.0.1:${port}/up"; then
  echo "server did not become ready on port ${port}" >&2
  cat "${server_log}" >&2
  exit 1
fi

(
  rss_started_at_ms="$(date +%s%3N)"
  while kill -0 "${server_pid}" 2>/dev/null; do
    rss_now_ms="$(date +%s%3N)"
    rss_kb="$(ps -o rss= -p "${server_pid}" 2>/dev/null | awk 'NF { print $1 }')"
    if [ -n "${rss_kb}" ]; then
      printf '%s\n' "${rss_kb}" >> "${rss_file}"
      jq -nc \
        --argjson t_ms "$(( rss_now_ms - rss_started_at_ms ))" \
        --argjson rss_mb "$(awk "BEGIN { printf \"%.4f\", ${rss_kb} / 1024 }")" \
        '{t_ms: $t_ms, rss_mb: $rss_mb}' >> "${rss_jsonl}"
    fi
    sleep 0.2
  done
) &
sampler_pid="$!"

curl --silent --show-error --fail --output /dev/null "${raw_url}"
curl --silent --show-error --fail --output /dev/null "${shaped_url}"

hyperfine_summary_json() {
  jq -c '
    .results[0]
    | {
        runs: (.times | length),
        mean_ms: (.mean * 1000),
        stddev_ms: (.stddev * 1000),
        median_ms: (.median * 1000),
        min_ms: (.min * 1000),
        max_ms: (.max * 1000),
        user_ms: (.user * 1000),
        system_ms: (.system * 1000)
      }
  ' "$1"
}

print_hyperfine_details() {
  jq -r '
    "  runs: \(.runs)\n" +
    "  mean_ms: \(.mean_ms)\n" +
    "  stddev_ms: \(.stddev_ms)\n" +
    "  median_ms: \(.median_ms)\n" +
    "  min_ms: \(.min_ms)\n" +
    "  max_ms: \(.max_ms)\n" +
    "  user_ms: \(.user_ms)\n" +
    "  system_ms: \(.system_ms)"
  '
}

run_hyperfine() {
  local label="$1"
  local url="$2"
  local json_file="$3"

  echo "${label}_timing:"
  hyperfine \
    --style full \
    --warmup "${hyperfine_warmup_runs}" \
    --runs "${hyperfine_runs}" \
    --time-unit millisecond \
    --export-json "${json_file}" \
    "curl --silent --show-error --fail --output /dev/null '${url}'"

  local summary_json
  summary_json="$(hyperfine_summary_json "${json_file}")"
  echo
  echo "${label}_timing_details:"
  printf '%s\n' "${summary_json}" | print_hyperfine_details
  printf '%s\n' "${summary_json}"
}

run_load_json() {
  local url="$1"
  local concurrency="$2"
  local status_file
  local started_ns
  local elapsed_ns
  local elapsed_secs
  local completed
  local errors
  local req_per_sec
  local status_counts_lines
  local status_counts_json
  status_file="$(mktemp)"

  started_ns="$(date +%s%N)"
  seq "${requests}" | xargs -P "${concurrency}" -I{} \
    sh -c "curl --silent --show-error --output /dev/null --write-out '%{http_code}\n' '${url}' || echo 000" \
    >> "${status_file}"
  elapsed_ns="$(( $(date +%s%N) - started_ns ))"
  elapsed_secs="$(awk "BEGIN { printf \"%.6f\", ${elapsed_ns} / 1000000000 }")"
  completed="$(wc -l < "${status_file}" | tr -d ' ')"
  errors="$(awk '$1 != 200 { count += 1 } END { print count + 0 }' "${status_file}")"
  req_per_sec="$(awk "BEGIN { printf \"%.2f\", ${completed} / ${elapsed_secs} }")"
  status_counts_lines="$(awk '{ counts[$1] += 1 } END { for (code in counts) print code, counts[code] }' "${status_file}" | sort -n)"
  status_counts_json="$(printf '%s\n' "${status_counts_lines}" | jq -Rn '
    [inputs | select(length > 0) | split(" ") | {(.[0]): (.[1] | tonumber)}] | add // {}
  ')"

  jq -n \
    --argjson concurrency "${concurrency}" \
    --argjson completed "${completed}" \
    --argjson errors "${errors}" \
    --argjson elapsed_secs "${elapsed_secs}" \
    --argjson req_per_sec "${req_per_sec}" \
    --argjson status_counts "${status_counts_json}" \
    '{
      concurrency: $concurrency,
      completed: $completed,
      errors: $errors,
      elapsed_secs: $elapsed_secs,
      req_per_sec: $req_per_sec,
      status_counts: $status_counts
    }'

  rm -f "${status_file}"
}

echo "sheet_id: ${TEST_SHEET_ID}"
echo "sheet_name: ${sheet_name}"
echo "raw_url: ${raw_url}"
echo "shaped_url: ${shaped_url}"
echo "requests_per_level: ${requests}"
echo "concurrency_levels: ${levels_csv}"
echo

raw_timing_json="$(run_hyperfine raw "${raw_url}" "${raw_hyperfine_json}" | tee /dev/stderr | tail -n1)"
echo
shaped_timing_json="$(run_hyperfine shaped "${shaped_url}" "${shaped_hyperfine_json}" | tee /dev/stderr | tail -n1)"
echo

single_request_json="$(jq -n \
  --argjson raw "${raw_timing_json}" \
  --argjson shaped "${shaped_timing_json}" \
  '{
    raw: $raw,
    shaped: $shaped,
    overhead_ms: ($shaped.mean_ms - $raw.mean_ms),
    ratio: (if $raw.mean_ms == 0 then 0 else $shaped.mean_ms / $raw.mean_ms end)
  }'
)"

echo "single_request_overhead_ms: $(printf '%s' "${single_request_json}" | jq -r '.overhead_ms')"
echo
echo "load_matrix:"

OLDIFS="$IFS"
IFS=','
for concurrency in ${levels_csv}; do
  IFS="$OLDIFS"
  echo
  echo "  concurrency: ${concurrency}"

  raw_load_json="$(run_load_json "${raw_url}" "${concurrency}")"
  shaped_load_json="$(run_load_json "${shaped_url}" "${concurrency}")"
  load_entry_json="$(jq -n \
    --argjson concurrency "${concurrency}" \
    --argjson raw "${raw_load_json}" \
    --argjson shaped "${shaped_load_json}" \
    '{
      concurrency: $concurrency,
      raw: $raw,
      shaped: $shaped,
      overhead: {
        req_per_sec_delta: ($shaped.req_per_sec - $raw.req_per_sec),
        req_per_sec_ratio: (if $raw.req_per_sec == 0 then 0 else $shaped.req_per_sec / $raw.req_per_sec end)
      }
    }'
  )"
  printf '%s\n' "${load_entry_json}" >> "${load_entries_jsonl}"

  echo "  raw:"
  printf '%s\n' "${raw_load_json}" | jq -r '
    "    completed: \(.completed)\n" +
    "    errors: \(.errors)\n" +
    "    elapsed_secs: \(.elapsed_secs)\n" +
    "    req_per_sec: \(.req_per_sec)\n" +
    "    status_counts:\n" +
    (.status_counts | to_entries | map("      " + .key + ": " + (.value|tostring)) | join("\n"))
  '

  echo "  shaped:"
  printf '%s\n' "${shaped_load_json}" | jq -r '
    "    completed: \(.completed)\n" +
    "    errors: \(.errors)\n" +
    "    elapsed_secs: \(.elapsed_secs)\n" +
    "    req_per_sec: \(.req_per_sec)\n" +
    "    status_counts:\n" +
    (.status_counts | to_entries | map("      " + .key + ": " + (.value|tostring)) | join("\n"))
  '

  echo "  overhead:"
  printf '%s\n' "${load_entry_json}" | jq -r '
    "    req_per_sec_delta: \(.overhead.req_per_sec_delta)\n" +
    "    req_per_sec_ratio: \(.overhead.req_per_sec_ratio)"
  '
done
IFS="$OLDIFS"

load_matrix_json="$(jq -s '.' "${load_entries_jsonl}")"
rss_samples_json="$(jq -s '.' "${rss_jsonl}")"
peak_rss_mb="$(awk 'BEGIN { max = 0 } $1 > max { max = $1 } END { printf "%.2f", max / 1024 }' "${rss_file}")"
avg_rss_mb="$(awk 'BEGIN { sum = 0; count = 0 } { sum += $1; count += 1 } END { if (count == 0) printf "0.00"; else printf "%.2f", (sum / count) / 1024 }' "${rss_file}")"
rss_start_mb="$(awk 'NF { printf "%.2f", $1 / 1024; exit }' "${rss_file}")"
rss_end_mb="$(awk 'NF { value = $1 } END { if (value == "") printf "0.00"; else printf "%.2f", value / 1024 }' "${rss_file}")"

jq -n \
  --arg file "${result_basename}" \
  --argjson timestamp "${timestamp}" \
  --arg sheet_name "${sheet_name}" \
  --arg raw_url "${raw_url}" \
  --arg shaped_url "${shaped_url}" \
  --argjson requests_per_level "${requests}" \
  --argjson concurrency_levels "$(printf '%s\n' "${levels_csv}" | jq -Rc 'split(",") | map(tonumber)')" \
  --argjson single_request "${single_request_json}" \
  --argjson load_matrix "${load_matrix_json}" \
  --argjson rss_samples "${rss_samples_json}" \
  --argjson peak_rss_mb "${peak_rss_mb}" \
  --argjson avg_rss_mb "${avg_rss_mb}" \
  --argjson rss_start_mb "${rss_start_mb}" \
  --argjson rss_end_mb "${rss_end_mb}" \
  '{
    file: $file,
    timestamp: $timestamp,
    sheet_name: $sheet_name,
    raw_url: $raw_url,
    shaped_url: $shaped_url,
    requests_per_level: $requests_per_level,
    concurrency_levels: $concurrency_levels,
    single_request: $single_request,
    load_matrix: $load_matrix,
    rss_samples: $rss_samples,
    peak_rss_mb: $peak_rss_mb,
    avg_rss_mb: $avg_rss_mb,
    rss_start_mb: $rss_start_mb,
    rss_end_mb: $rss_end_mb
  }' > "${result_file}"

find "${results_dir}" -maxdepth 1 -type f -name 'run-*.json' | sort \
  | xargs jq -s '
      sort_by(.timestamp)
      | reverse
      | map({
          file,
          timestamp,
          sheet_name,
          requests_per_level,
          concurrency_levels,
          single_request_overhead_ms: .single_request.overhead_ms,
          peak_rss_mb,
          avg_rss_mb,
          rss_start_mb,
          rss_end_mb
        })
    ' > "${results_dir}/index.json"

echo
echo "peak_rss_mb: ${peak_rss_mb}"
echo "result_file: ${result_file}"
echo "result_index: ${results_dir}/index.json"

viewer_port="${BENCH_VIEWER_PORT:-8181}"
viewer_url="http://127.0.0.1:${viewer_port}/compare.html"

python3 -m http.server -d "${results_dir}" "${viewer_port}" > "${viewer_log}" 2>&1 &
viewer_pid="$!"
viewer_ready=0

for _ in $(seq 1 50); do
  if ! kill -0 "${viewer_pid}" 2>/dev/null; then
    echo "results viewer failed to start" >&2
    cat "${viewer_log}" >&2
    exit 1
  fi
  if curl --silent --fail --output /dev/null "${viewer_url}"; then
    viewer_ready=1
    break
  fi
  sleep 0.1
done

if [ "${viewer_ready}" -ne 1 ]; then
  echo "results viewer did not become ready on port ${viewer_port}" >&2
  cat "${viewer_log}" >&2
  exit 1
fi

echo "viewer_url: ${viewer_url}"

if command -v xdg-open > /dev/null 2>&1; then
  xdg-open "${viewer_url}" > /dev/null 2>&1 || true
elif command -v open > /dev/null 2>&1; then
  open "${viewer_url}" > /dev/null 2>&1 || true
fi

echo "viewer_pid: ${viewer_pid}"
echo "Press Ctrl+C to stop the local results server."
wait "${viewer_pid}"
