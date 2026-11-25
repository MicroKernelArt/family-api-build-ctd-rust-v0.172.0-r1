#!/usr/bin/env bash
# Build profile contract shared by the sidecar entrypoint and its tests.
# shellcheck disable=SC2034 # Values are exported to scripts that source this library.

RUST_IMAGE="rust:1.95-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1"
DEBIAN_IMAGE="debian:trixie-slim@sha256:3a39a0592364683e6bab97937b72cad5a8fa6dcbbee90edb3bb48c7f8e94f258"

ctd_configure_build_profile() {
  local repo="$1"
  local manifest="$repo/patches/transportd/series.yaml"
  local local_state_root
  local manifest_release_id
  local resolved_receipt
  local resolved_repo

  BUILD_PROFILE="${BUILD_PROFILE:-release}"
  manifest_release_id="$(sed -n 's/^release_id: //p' "$manifest" | tr -d ' ')"
  [ -n "$manifest_release_id" ] || {
    echo "FAIL: series.yaml missing release_id" >&2
    return 1
  }

  case "$BUILD_PROFILE" in
    local)
      PLATFORM="${PLATFORM:-linux/arm64}"
      [ "$PLATFORM" = "linux/arm64" ] || {
        echo "FAIL: local profile only supports linux/arm64 (actual: $PLATFORM)" >&2
        return 1
      }
      TRIPLE="aarch64-unknown-linux-gnu"
      DEB_ARCH="arm64"
      EXPECTED_UNAME="aarch64"
      VENDOR_DEBS_REL="deploy/vendor-debs-arm64"
      SOURCE_ID_KIND="invocation"
      IMAGE_TAG="${IMAGE_TAG:-weishaw/codex-transportd:local-arm64}"
      case "$IMAGE_TAG" in
        *:local|*:local-*) : ;;
        *)
          echo "FAIL: local profile image tag must use the :local namespace: $IMAGE_TAG" >&2
          return 1
          ;;
      esac
      RELEASE_ID="${RELEASE_ID:-${manifest_release_id}-local-dev}"
      case "$RELEASE_ID" in
        *-dev) : ;;
        *)
          echo "FAIL: local profile release id must end in -dev: $RELEASE_ID" >&2
          return 1
          ;;
      esac
      local_state_root="${CTD_LOCAL_STATE:-$HOME/.cache/codex-transportd}"
      RECEIPT_PATH="$local_state_root/receipts/local-arm64.txt"
      DEFAULT_TARGET_CACHE="$local_state_root/target/arm64"
      ;;
    release)
      PLATFORM="${PLATFORM:-linux/amd64}"
      [ "$PLATFORM" = "linux/amd64" ] || {
        echo "FAIL: release profile only supports linux/amd64 (actual: $PLATFORM)" >&2
        return 1
      }
      TRIPLE="x86_64-unknown-linux-gnu"
      DEB_ARCH="amd64"
      EXPECTED_UNAME="x86_64"
      VENDOR_DEBS_REL="deploy/vendor-debs"
      SOURCE_ID_KIND="manifest"
      IMAGE_TAG="${IMAGE_TAG:-weishaw/codex-transportd:build}"
      RELEASE_ID="${RELEASE_ID:-$manifest_release_id}"
      [ "$RELEASE_ID" = "$manifest_release_id" ] || {
        echo "FAIL: release profile release id must match manifest: $manifest_release_id" >&2
        return 1
      }
      RECEIPT_PATH="${CTD_RELEASE_RECEIPT_PATH:-}"
      [ -n "$RECEIPT_PATH" ] || {
        echo "FAIL: release profile requires CTD_RELEASE_RECEIPT_PATH" >&2
        return 1
      }
      case "$RECEIPT_PATH" in
        /*) ;;
        *) echo "FAIL: CTD_RELEASE_RECEIPT_PATH must be absolute" >&2; return 1 ;;
      esac
      resolved_repo="$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$repo")"
      resolved_receipt="$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$RECEIPT_PATH")"
      case "$resolved_receipt" in
        "$resolved_repo"|"$resolved_repo"/*)
          echo "FAIL: CTD_RELEASE_RECEIPT_PATH must be outside the source tree" >&2
          return 1
          ;;
      esac
      DEFAULT_TARGET_CACHE=""
      ;;
    *)
      echo "FAIL: BUILD_PROFILE must be local or release (actual: '$BUILD_PROFILE')" >&2
      return 1
      ;;
  esac
}

ctd_assert_native_build_daemon() {
  local daemon_arch
  daemon_arch="$(docker info --format '{{.Architecture}}' 2>/dev/null)" || {
    echo "FAIL: cannot read Docker daemon architecture" >&2
    return 1
  }
  case "$BUILD_PROFILE:$daemon_arch" in
    local:arm64|local:aarch64|release:amd64|release:x86_64) : ;;
    *)
      echo "FAIL: $BUILD_PROFILE profile requires a native ${PLATFORM#linux/} Docker daemon (actual: $daemon_arch)" >&2
      return 1
      ;;
  esac
}

ctd_resolve_source_commit() {
  local repo="$1"
  if [ "$SOURCE_ID_KIND" = "invocation" ]; then
    local dirty
    dirty="$(git -C "$repo" status --porcelain)"
    [ -z "$dirty" ] || {
      echo "FAIL: local source requires a clean worktree" >&2
      return 1
    }
    git -C "$repo" rev-parse HEAD
  else
    sed -n 's/^source_commit: //p' "$repo/patches/transportd/series.yaml" | tr -d ' '
  fi
}
