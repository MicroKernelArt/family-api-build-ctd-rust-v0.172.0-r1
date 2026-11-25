#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"

require() {
  local label="$1"
  local needle="$2"
  local file="$3"
  grep -qF "$needle" "$file" || {
    echo "FAIL: $label" >&2
    exit 1
  }
}

require "models must preserve upstream status" ".execute_raw_with(" "$REPO/codex-rs/codex-api/src/endpoint/models.rs"
require "search must preserve upstream status" ".execute_raw_with(" "$REPO/codex-rs/codex-api/src/endpoint/search.rs"
require "closed pong channel must terminate" "let Some(payload) = payload else { break };" "$REPO/codex-rs/codex-transportd/src/server/websocket.rs"
require "duplicate headers must append" "out.append(name.clone(), value.clone());" "$REPO/codex-rs/codex-transportd/src/contract/mod.rs"
require "UDS connection limit must exist" "MAX_UDS_CONNECTIONS" "$REPO/codex-rs/codex-transportd/src/limits.rs"
require "accept loop must acquire a connection permit" ".acquire_owned()" "$REPO/codex-rs/codex-transportd/src/server/mod.rs"
require "raw request logging must use common guard" "self.trace_request(&req);" "$REPO/codex-rs/http-client/src/transport.rs"

echo "PASS: consolidated transportd static invariants"
