#!/usr/bin/env bash
# local arm64 / release amd64 profile isolation and prepare wiring regression.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
PROFILE_LIB="$REPO/deploy/build-sidecar-profile-lib.sh"
BUILD_SCRIPT="$REPO/deploy/build-sidecar.sh"

[ -f "$PROFILE_LIB" ] || {
  echo "FAIL: missing production profile library: $PROFILE_LIB"
  exit 1
}

WORK="$(mktemp -d "${TMPDIR:-/tmp}/ctd-platform-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

assert_eq() {
  local actual="$1" expected="$2" label="$3"
  if [ "$actual" != "$expected" ]; then
    echo "FAIL: $label: actual='$actual' expected='$expected'"
    exit 1
  fi
}

profile_config() {
  local profile="$1" platform="${2:-}" image_tag="${3:-}" receipt="${4:-}"
  BUILD_PROFILE="$profile" PLATFORM="$platform" IMAGE_TAG="$image_tag" HOME="$WORK/home" \
    CTD_RELEASE_RECEIPT_PATH="$receipt" \
    bash -c '
      set -euo pipefail
      source "$1"
      ctd_configure_build_profile "$2"
      printf "%s|%s|%s|%s|%s|%s|%s|%s\n" \
        "$BUILD_PROFILE" "$PLATFORM" "$TRIPLE" "$DEB_ARCH" \
        "$VENDOR_DEBS_REL" "$SOURCE_ID_KIND" "$RECEIPT_PATH" "$DEFAULT_TARGET_CACHE"
    ' bash "$PROFILE_LIB" "$REPO"
}

local_config="$(profile_config local)"
assert_eq "$local_config" \
  "local|linux/arm64|aarch64-unknown-linux-gnu|arm64|deploy/vendor-debs-arm64|invocation|$WORK/home/.cache/codex-transportd/receipts/local-arm64.txt|$WORK/home/.cache/codex-transportd/target/arm64" \
  "local profile defaults"

release_receipt="$WORK/receipts/release.txt"
release_config="$(profile_config release "" "" "$release_receipt")"
assert_eq "$release_config" \
  "release|linux/amd64|x86_64-unknown-linux-gnu|amd64|deploy/vendor-debs|manifest|$release_receipt|" \
  "release profile defaults"

if profile_config release >/dev/null 2>&1; then
  echo "FAIL: release profile accepted a missing external receipt path"
  exit 1
fi
if profile_config release "" "" "$REPO/receipt.txt" >/dev/null 2>&1; then
  echo "FAIL: release profile accepted an in-tree receipt path"
  exit 1
fi
if profile_config release "" "" "$REPO/../$(basename "$REPO")/receipt.txt" >/dev/null 2>&1; then
  echo "FAIL: release profile accepted an in-tree receipt path containing traversal"
  exit 1
fi
ln -s "$REPO" "$WORK/repo-link"
if profile_config release "" "" "$WORK/repo-link/receipt.txt" >/dev/null 2>&1; then
  echo "FAIL: release profile accepted a symlinked in-tree receipt path"
  exit 1
fi

for invalid in "local linux/amd64" "release linux/arm64"; do
  invalid_profile="${invalid%% *}"
  invalid_platform="${invalid#* }"
  if profile_config "$invalid_profile" "$invalid_platform" >/dev/null 2>&1; then
    echo "FAIL: invalid profile/platform accepted: $invalid"
    exit 1
  fi
done
if profile_config local "" "weishaw/codex-transportd:build" >/dev/null 2>&1; then
  echo "FAIL: local profile accepted a release image tag"
  exit 1
fi

MOCK_BIN="$WORK/bin"
mkdir -p "$MOCK_BIN" "$WORK/home"

cat > "$MOCK_BIN/docker" <<'EOF'
#!/usr/bin/env bash
echo "$*" >> "$CTD_TEST_DOCKER_LOG"
if [ "${1:-}" = "info" ]; then
  echo "${CTD_TEST_DAEMON_ARCH:-arm64}"
fi
exit 0
EOF

cat > "$MOCK_BIN/curl" <<'EOF'
#!/usr/bin/env bash
echo "$*" >> "$CTD_TEST_CURL_LOG"
out=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then
    out="$2"
    shift 2
  else
    shift
  fi
done
[ -n "$out" ] || exit 2
printf 'test archive\n' > "$out"
EOF

cat > "$MOCK_BIN/shasum" <<'EOF'
#!/usr/bin/env bash
path="${@: -1}"
name="$(basename "$path")"
triple="${name#librusty_v8_release_}"
triple="${triple%.a.gz.tmp}"
triple="${triple%.a.gz}"
sha="$(sed -n "s/^v8_archive_sha256_${triple}: //p" "$CTD_TEST_SERIES" | tr -d ' ')"
[ -n "$sha" ] || exit 3
printf '%s  %s\n' "$sha" "$path"
EOF

cat > "$MOCK_BIN/git" <<'EOF'
#!/usr/bin/env bash
if [ "${1:-}" = "-C" ]; then
  shift 2
fi
case "${1:-}" in
  status) exit 0 ;;
  rev-parse) printf '1111111111111111111111111111111111111111\n' ;;
  clone)
    target="${@: -1}"
    mkdir -p "$target/codex-rs"
    ;;
esac
exit 0
EOF
chmod +x "$MOCK_BIN"/*
: > "$WORK/curl.log"

run_prepare() {
  local profile="$1" platform="${2:-}" daemon_arch="${3:-}" log="$WORK/docker-$1-${2:-default}.log"
  local receipt="$WORK/receipts/$profile-${2:-default}.txt"
  if [ -z "$daemon_arch" ]; then
    case "$profile" in
      local) daemon_arch="arm64" ;;
      release) daemon_arch="x86_64" ;;
    esac
  fi
  : > "$log"
  if ! env -i \
    PATH="$MOCK_BIN:$PATH" HOME="$WORK/home" TMPDIR="$WORK" LC_ALL=C.UTF-8 \
    BUILD_PROFILE="$profile" PLATFORM="$platform" \
    CTD_RELEASE_RECEIPT_PATH="$receipt" \
    CTD_V8_CACHE="$WORK/v8-$profile-${2:-default}" \
    CTD_TEST_DOCKER_LOG="$log" \
    CTD_TEST_CURL_LOG="$WORK/curl.log" \
    CTD_TEST_SERIES="$REPO/patches/transportd/series.yaml" \
    CTD_TEST_DAEMON_ARCH="$daemon_arch" \
    bash "$BUILD_SCRIPT" prepare >"$WORK/$profile-${2:-default}.out" 2>&1; then
    cat "$WORK/$profile-${2:-default}.out" >&2
    return 1
  fi
  printf '%s\n' "$log"
}

local_log="$(run_prepare local)"
grep -Fq "cargo fetch --locked（source=1111111111111111111111111111111111111111）" "$WORK/local-default.out" || {
  echo "FAIL: UTF-8 locale corrupted the source identity expansion"
  exit 1
}
grep -Fq "pull --platform linux/arm64 rust:1.95-bookworm@sha256:" "$local_log" || {
  echo "FAIL: local prepare did not pull arm64 rust image"
  exit 1
}
grep -Fq "pull --platform linux/arm64 debian:trixie-slim@sha256:" "$local_log" || {
  echo "FAIL: local prepare did not pull arm64 debian image"
  exit 1
}
if ! grep -Eq '^run --rm --pull=never --platform linux/arm64 -v .+ rust:1\.95-bookworm@sha256:[0-9a-f]+ bash -c ' "$local_log"; then
  echo "FAIL: prepare docker run options/mounts must precede the image"
  exit 1
fi
if ! grep -Fq "export PATH=\"/usr/local/rustup/toolchains/1.95.0-aarch64-unknown-linux-gnu/bin:\$PATH\"" "$local_log"; then
  echo "FAIL: prepare must use the image-pinned toolchain without rustup component sync"
  exit 1
fi
[ ! -e "$release_receipt" ] || {
  echo "FAIL: local prepare created a release receipt"
  exit 1
}
local_curl_before="$(wc -l < "$WORK/curl.log" | tr -d ' ')"
run_prepare local >/dev/null
local_curl_after="$(wc -l < "$WORK/curl.log" | tr -d ' ')"
assert_eq "$local_curl_after" "$local_curl_before" "valid V8 cache avoids another download"

release_log="$(run_prepare release)"
grep -Fq "pull --platform linux/amd64 rust:1.95-bookworm@sha256:" "$release_log" || {
  echo "FAIL: release prepare did not pull amd64 rust image"
  exit 1
}
grep -Fq "pull --platform linux/amd64 debian:trixie-slim@sha256:" "$release_log" || {
  echo "FAIL: release prepare did not pull amd64 debian image"
  exit 1
}

release_arm_log="$WORK/docker-release-arm64-daemon.log"
: > "$release_arm_log"
if env -i \
  PATH="$MOCK_BIN:$PATH" HOME="$WORK/home" TMPDIR="$WORK" LC_ALL=C.UTF-8 \
  BUILD_PROFILE=release CTD_RELEASE_RECEIPT_PATH="$release_receipt" \
  CTD_TEST_DAEMON_ARCH=arm64 CTD_TEST_DOCKER_LOG="$release_arm_log" \
  CTD_TEST_CURL_LOG="$WORK/curl.log" CTD_TEST_SERIES="$REPO/patches/transportd/series.yaml" \
  bash "$BUILD_SCRIPT" prepare >/dev/null 2>&1; then
  echo "FAIL: release prepare accepted an arm64 Docker daemon"
  exit 1
fi
assert_eq "$(cat "$release_arm_log")" "info --format {{.Architecture}}" \
  "release on arm64 daemon fails before image pulls or containers"

for invalid in "local linux/amd64" "release linux/arm64"; do
  invalid_profile="${invalid%% *}"
  invalid_platform="${invalid#* }"
  invalid_log="$WORK/docker-invalid-$invalid_profile-${invalid_platform#linux/}.log"
  : > "$invalid_log"
  if env -i \
    PATH="$MOCK_BIN:$PATH" HOME="$WORK/home" TMPDIR="$WORK" LC_ALL=C.UTF-8 \
    BUILD_PROFILE="$invalid_profile" PLATFORM="$invalid_platform" CTD_TEST_DOCKER_LOG="$invalid_log" \
    CTD_TEST_CURL_LOG="$WORK/curl.log" CTD_TEST_SERIES="$REPO/patches/transportd/series.yaml" \
    bash "$BUILD_SCRIPT" prepare >/dev/null 2>&1; then
    echo "FAIL: production entrypoint accepted invalid combination: $invalid"
    exit 1
  fi
  assert_eq "$(wc -l < "$invalid_log" | tr -d ' ')" "0" "invalid combination has zero Docker calls"
done

echo "build-sidecar platform profile test passed"
