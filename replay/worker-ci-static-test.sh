#!/bin/sh
# shellcheck disable=SC2016 # Static assertions intentionally match literal shell variables.
set -eu

repo=$(unset CDPATH; cd -- "$(dirname -- "$0")/.." && pwd)
config="$repo/replay/worker-ci.json"
workflow="$repo/.github/workflows/worker-ci.yml"
policy_script="$repo/codex-rs/scripts/check-platform-archives.sh"

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    exit 1
}

require_text() {
    file=$1
    needle=$2
    grep -qF -- "$needle" "$file" || fail "$file missing: $needle"
}

test -f "$config" || fail 'worker CI config is missing'
test -f "$workflow" || fail 'worker CI workflow is missing'
test -f "$policy_script" || fail 'platform archive policy script is missing'
test -x "$policy_script" || fail 'platform archive policy script is not executable'

# R3-WINDOWS-001：每个 tracked path 必须能在 Windows 上检出——路径段含
# ':' 等 Windows 非法字符时 actions/checkout 直接失败（git exit 128）。
# '/' 是 POSIX 分隔符本身，不在非法字符类内；逐段非法即整条路径非法。
invalid_paths=$(
    git -C "$repo" ls-files | grep -E '[<>:"|?*\\]|[[:cntrl:]]' || true
)
if [ -n "$invalid_paths" ]; then
    printf 'FAIL: tracked paths contain Windows-invalid characters:\n%s\n' \
        "$invalid_paths" >&2
    exit 1
fi

python3 - "$config" <<'PY'
import json
import sys

config = json.load(open(sys.argv[1], encoding="utf-8"))
assert config == {
    "schema": "cwf/worker-github-ci-config/v1",
    "project": "family-api-codex",
    "repo": "MicroKernelArt/family-api-codex",
    "required_workflows": ["worker-ci.yml"],
    "max_fix_rounds": 3,
}
PY

require_text "$workflow" 'branches: ["ci/family-api-codex/**"]'
require_text "$workflow" "CWF_CI_IDENTITY repo=\${GITHUB_REPOSITORY} head=\${HEAD} tree=\${TREE} branch=\${GITHUB_REF_NAME}"
for job in identity transportd replay-gates platform-policy results; do
    require_text "$workflow" "  $job:"
done
require_text "$workflow" 'cargo build --locked -p codex-transportd --release'
require_text "$workflow" 'cargo test --locked -p codex-transportd'
require_text "$workflow" 'cargo test --locked -p codex-transportd --features test-upstream'
require_text "$workflow" 'cargo test --locked -p codex-client --test retry_raw_tests'
require_text "$workflow" 'cargo test --locked -p codex-http-client --lib'
require_text "$workflow" 'cargo clippy --locked -p codex-transportd --no-deps -- -D warnings'
require_text "$workflow" 'bash replay/gate.sh'
require_text "$workflow" 'bash patches/transportd/tools/export-gate.sh'
require_text "$workflow" 'bash patches/transportd/tools/release-evidence-gate.sh --receipt'
require_text "$workflow" 'bash codex-rs/scripts/check-platform-archives.sh'
# R3-LINK-001 回归（静态）：连续两次 hosted run（33472913683、33498330072）
# 在同一 final lib-test link 阶段死于 ld signal 7 [Bus error]，两次之间
# 零产品变更。CI-only linker 资源策略必须保留：dev/test debuginfo 关闭 +
# transportd 与 replay-gates 两个 job 均带 6G swap 上限步骤。release
# profile 与 receipt 绑定的 `cargo build --release` 不受影响。
require_text "$workflow" 'CARGO_PROFILE_DEV_DEBUG: "0"'
require_text "$workflow" 'CARGO_PROFILE_TEST_DEBUG: "0"'
require_text "$workflow" 'CARGO_TARGET_DIR: /mnt/ci-target'
require_text "$workflow" 'sudo mkdir -p /mnt/ci-target'
require_text "$workflow" 'sudo swapon /swapfile'
require_text "$workflow" 'R3-LINK-001'
swap_steps=$(grep -cF 'sudo swapon /swapfile' "$workflow") || swap_steps=0
[ "$swap_steps" -eq 2 ] ||
    fail "linker swap step must appear exactly in transportd and replay-gates jobs (found $swap_steps)"
# release profile artifacts are disposable once the release build proves:
# they must be dropped before the test-profile build so both profiles never
# coexist on the runner disk (R3-LINK-001 os 28 evidence).
require_text "$workflow" 'rm -rf "${CARGO_TARGET_DIR}/release"'

if grep -qE -- 'docker push|gh release|git push|refs/tags/' "$workflow"; then
    fail 'worker CI workflow contains a publication or tag command'
fi
if grep -qF 'runs_on:' "$workflow" || grep -qF -- '-runners' "$workflow"; then
    fail 'worker CI workflow depends on a repository self-hosted runner group'
fi

require_text "$repo/replay/gate.sh" 'merge-base --is-ancestor "$SOURCE_COMMIT" "$RELEASE_COMMIT"'
require_text "$repo/replay/gate.sh" 'merge-base --is-ancestor "$RELEASE_COMMIT" "$CURRENT"'
require_text "$repo/replay/gate.sh" 'post-source review commit changes non-evidence path'
require_text "$repo/.github/workflows/transportd.yml" 'branches: ["transportd/**", "replay_*"]'
require_text "$repo/.github/workflows/transportd-release-gates.yml" 'branches: ["transportd/**", "replay_*"]'

python3 - "$repo" "$workflow" "$repo/.github/workflows/transportd.yml" "$repo/.github/workflows/transportd-release-gates.yml" <<'PY'
import re
import sys
from pathlib import Path

repo = Path(sys.argv[1])
workflow_paths = [Path(path) for path in sys.argv[2:]]
sha = re.compile(r"^[0-9a-f]{40}$")
script_command = re.compile(
    r"^\s*(?:run:\s*)?(?:bash|sh|python3|python)\s+"
    r"((?:\./)?[A-Za-z0-9_.-]+(?:/[A-Za-z0-9_.-]+)*\.(?:sh|py))(?:\s|$)"
)
action = re.compile(r"^\s*-?\s*uses:\s*(\S+)")
floating_tool = re.compile(
    r"\b[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+@(?:latest|master|main|v[0-9])"
)

for workflow_path in workflow_paths:
    text = workflow_path.read_text(encoding="utf-8")
    for line in text.splitlines():
        action_match = action.match(line)
        if action_match:
            reference = action_match.group(1)
            if not reference.startswith("./"):
                provider, separator, revision = reference.partition("@")
                assert separator and provider and sha.fullmatch(revision), (
                    f"floating or malformed Action reference in {workflow_path}: {reference}"
                )
        command_match = script_command.match(line)
        if command_match:
            script = command_match.group(1)
            assert (repo / script.removeprefix("./")).is_file(), (
                f"workflow-referenced script is missing in {workflow_path}: {script}"
            )
    floating = floating_tool.findall(text)
    assert not floating, f"floating CI tool reference in {workflow_path}: {floating}"
PY

printf 'PASS: family-api-codex worker CI contract\n'
