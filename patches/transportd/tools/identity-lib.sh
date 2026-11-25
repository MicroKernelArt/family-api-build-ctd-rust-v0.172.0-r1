#!/usr/bin/env bash

ctd_repo_root() {
  cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd
}

ctd_field() {
  local file="$1"
  local key="$2"
  sed -n "s/^${key}:[[:space:]]*//p" "$file" | head -1
}

ctd_sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

ctd_require_hex() {
  local label="$1"
  local value="$2"
  local length="$3"
  case "$value" in
    *[!0-9a-f]*) echo "FAIL: $label is not lowercase hexadecimal: $value" >&2; return 1 ;;
  esac
  [ "${#value}" -eq "$length" ] || {
    echo "FAIL: $label must contain $length hexadecimal characters: $value" >&2
    return 1
  }
}

ctd_require_external_receipt() {
  local repo="$1"
  local receipt="$2"
  local resolved_repo
  local resolved_receipt
  case "$receipt" in
    /*) ;;
    *) echo "FAIL: receipt path must be absolute: $receipt" >&2; return 1 ;;
  esac
  resolved_repo="$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$repo")"
  resolved_receipt="$(python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$receipt")"
  case "$resolved_receipt" in
    "$resolved_repo"|"$resolved_repo"/*)
      echo "FAIL: receipt path must be outside the source tree: $receipt" >&2
      return 1
      ;;
  esac
}
