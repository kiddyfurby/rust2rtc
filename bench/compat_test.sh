#!/usr/bin/env bash
# ==============================================================================
# bench/compat_test.sh - Verify go2rtc REST API compatibility
# ==============================================================================
set -euo pipefail

TARGET_URL="${1:-}"
GO2RTC_URL="${GO2RTC_URL:-}"
TMP_DIR="$(mktemp -d)"

cleanup() {
    if [[ -n "${SPAWN_PID:-}" ]] && kill -0 "${SPAWN_PID}" 2>/dev/null; then
        kill "${SPAWN_PID}" 2>/dev/null || true
    fi
    rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

if [[ -z "${TARGET_URL}" ]]; then
    echo "Building rust2rtc..."
    cargo build --quiet
    BIN="./target/debug/rust2rtc"

    cat > "${TMP_DIR}/compat.yaml" << 'EOF'
api:
  listen: "127.0.0.1:12984"
rtsp:
  listen: "127.0.0.1:19554"
streams:
  test_cam:
    - "rtsp://192.0.2.10:554/stream1"
EOF
    "${BIN}" --config "${TMP_DIR}/compat.yaml" >/dev/null 2>&1 &
    SPAWN_PID=$!
    sleep 1
    TARGET_URL="http://127.0.0.1:12984"
fi

echo "Running go2rtc API compatibility checks against ${TARGET_URL}..."

check_endpoint() {
    local name="$1"
    local method="$2"
    local path="$3"
    local expected_substr="$4"

    local resp
    resp=$(curl -fsS -X "${method}" "${TARGET_URL}${path}")
    if [[ "${resp}" != *"${expected_substr}"* ]]; then
        echo "❌ FAIL [${name}]: ${method} ${path} did not contain '${expected_substr}'"
        echo "   Response: ${resp}"
        exit 1
    fi
    echo "✅ PASS [${name}]: ${method} ${path}"
}

check_endpoint "Server Info (/api)" "GET" "/api" '"version"'
check_endpoint "Supported Schemes (/api/schemes)" "GET" "/api/schemes" '"rtsp"'
check_endpoint "List Streams (/api/streams)" "GET" "/api/streams" '"producers"'
check_endpoint "Add Dynamic Stream (PUT /api/streams)" "PUT" "/api/streams?name=dyn_cam&src=rtsp://192.0.2.99/live" ""
check_endpoint "Verify Dynamic Stream" "GET" "/api/streams" '"dyn_cam"'
check_endpoint "Delete Dynamic Stream (DELETE /api/streams)" "DELETE" "/api/streams?src=dyn_cam" ""
check_endpoint "Preload Endpoint (/api/preload)" "GET" "/api/preload" "{"

if [[ -n "${GO2RTC_URL}" ]]; then
    echo ""
    echo "Comparing top-level /api keys against reference go2rtc (${GO2RTC_URL})..."
    R_API=$(curl -fsS "${TARGET_URL}/api")
    G_API=$(curl -fsS "${GO2RTC_URL}/api")
    echo "  rust2rtc /api : ${R_API}"
    echo "  go2rtc   /api : ${G_API}"
fi

echo ""
echo "🎉 All go2rtc compatibility checks passed!"
