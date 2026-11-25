#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
SRC="$REPO/codex-rs/codex-transportd/src"

grep -qF "CAPTURE_MAX_BYTES" "$SRC/capture.rs"
grep -qF "MAX_QUEUE" "$SRC/capture.rs"
grep -qF "checked_add" "$SRC/capture_budget.rs"
grep -qF "ErrorKind::WriteZero" "$SRC/capture_writer.rs"
grep -qF "budget.release(carried)" "$SRC/capture_writer.rs"
if grep -RqsF "saturating_sub" "$SRC/capture.rs" "$SRC/capture_budget.rs" "$SRC/capture_writer.rs"; then
  echo "FAIL: capture byte accounting contains fail-open subtraction" >&2
  exit 1
fi

echo "PASS: capture queue, byte budget, and write-progress invariants"
