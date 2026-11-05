#!/usr/bin/env bash
# ==============================================================================
# bench/measure_rss.sh - Reproducible RSS & CPU measurement for rust2rtc / go2rtc
# ==============================================================================
set -euo pipefail

RUST2RTC_BIN="${1:-./target/release/rust2rtc}"
GO2RTC_BIN="${GO2RTC_BIN:-}"

if [[ ! -x "${RUST2RTC_BIN}" ]]; then
    echo "Building release binary first..."
    cargo build --release
fi

TMP_DIR="$(mktemp -d)"
cleanup() {
    if [[ -n "${R_PID:-}" ]] && kill -0 "${R_PID}" 2>/dev/null; then
        kill "${R_PID}" 2>/dev/null || true
    fi
    if [[ -n "${G_PID:-}" ]] && kill -0 "${G_PID}" 2>/dev/null; then
        kill "${G_PID}" 2>/dev/null || true
    fi
    rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

cat > "${TMP_DIR}/bench_config.yaml" << 'EOF'
api:
  listen: "127.0.0.1:11984"
rtsp:
  listen: "127.0.0.1:18554"
streams:
  cam1: "rtsp://127.0.0.1:18554/ synthetic1"
  cam2: "rtsp://127.0.0.1:18554/ synthetic2"
  cam3: "rtsp://127.0.0.1:18554/ synthetic3"
  cam4: "rtsp://127.0.0.1:18554/ synthetic4"
  cam5: "rtsp://127.0.0.1:18554/ synthetic5"
  cam6: "rtsp://127.0.0.1:18554/ synthetic6"
  cam7: "rtsp://127.0.0.1:18554/ synthetic7"
EOF

measure_proc() {
    local pid="$1"
    local rss_kb
    rss_kb=$(ps -o rss= -p "${pid}" | tr -d ' ')
    local cpu_pct
    cpu_pct=$(ps -o %cpu= -p "${pid}" | tr -d ' ')
    awk -v kb="${rss_kb}" -v cpu="${cpu_pct}" 'BEGIN { printf "%.2f MB (RSS: %d KB, CPU: %s%%)", kb/1024.0, kb, cpu }'
}

echo "Starting rust2rtc (${RUST2RTC_BIN})..."
"${RUST2RTC_BIN}" --config "${TMP_DIR}/bench_config.yaml" >/dev/null 2>&1 &
R_PID=$!
sleep 2

R_IDLE=$(measure_proc "${R_PID}")
curl -fsS "http://127.0.0.1:11984/api/streams" >/dev/null
R_AFTER_API=$(measure_proc "${R_PID}")
kill "${R_PID}" 2>/dev/null || true
wait "${R_PID}" 2>/dev/null || true
unset R_PID

echo ""
echo "=================================================================="
echo "Benchmark Results ($(uname -s) $(uname -m))"
echo "=================================================================="
echo "rust2rtc Idle        : ${R_IDLE}"
echo "rust2rtc After API   : ${R_AFTER_API}"

if [[ -n "${GO2RTC_BIN}" && -x "${GO2RTC_BIN}" ]]; then
    "${GO2RTC_BIN}" -config "${TMP_DIR}/bench_config.yaml" >/dev/null 2>&1 &
    G_PID=$!
    sleep 2
    G_IDLE=$(measure_proc "${G_PID}")
    curl -fsS "http://127.0.0.1:11984/api/streams" >/dev/null
    G_AFTER_API=$(measure_proc "${G_PID}")
    kill "${G_PID}" 2>/dev/null || true
    wait "${G_PID}" 2>/dev/null || true
    unset G_PID
    echo "go2rtc Idle          : ${G_IDLE}"
    echo "go2rtc After API     : ${G_AFTER_API}"
fi
echo "=================================================================="
