#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$REPO"

python3 - "$REPO" <<'PY'
import re
import stat
import subprocess
import sys
from pathlib import Path

repo = Path(sys.argv[1])
inputs = [
    repo / ".github/workflows/worker-ci.yml",
    repo / ".github/workflows/transportd-release-gates.yml",
    repo / "RUNBOOK.md",
    *sorted((repo / "deploy").glob("*.sh")),
]
command = re.compile(
    r"(?:^|[;&|]\s*|\brun:\s*|\s)(?:bash|sh)\s+"
    r'["\']?(?:\$REPO/)?'
    r"((?:\./)?(?:patches|replay|deploy|codex-rs)/[A-Za-z0-9_./-]+\.sh)"
)
referenced = set()
for source in inputs:
    if not source.is_file():
        raise SystemExit(f"FAIL: static-check input is missing: {source.relative_to(repo)}")
    for match in command.finditer(source.read_text(encoding="utf-8")):
        referenced.add(match.group(1).removeprefix("./"))

required_tools = {
    "patches/transportd/tools/export-gate.sh",
    "patches/transportd/tools/release-evidence-gate.sh",
    "patches/transportd/tools/write-source-receipt.sh",
    "patches/transportd/tools/ci-gates-static-test.sh",
    "patches/transportd/tools/static-fix-gates.sh",
    "patches/transportd/tools/capture-isolation-gate.sh",
    "patches/transportd/tools/gate-mutation-test.sh",
    "patches/transportd/tools/gate-export-mutation-test.sh",
    "patches/transportd/tools/gate-evidence-mutation-test.sh",
    "patches/transportd/tools/gate-wire-sbom-mutation-test.sh",
    "patches/transportd/tools/materialize-release-artifact.sh",
    "patches/transportd/tools/bin-verify.sh",
    "patches/transportd/tools/release-tag-verify.sh",
    "patches/transportd/tools/wire-gate.sh",
    "patches/transportd/tools/sbom-gate.sh",
}
for relative in sorted(required_tools):
    for match in command.finditer((repo / relative).read_text(encoding="utf-8")):
        referenced.add(match.group(1).removeprefix("./"))
missing_references = required_tools - referenced
if missing_references:
    raise SystemExit(f"FAIL: expected gate commands are not referenced: {sorted(missing_references)}")

repo_file = re.compile(
    r"\$REPO/([A-Za-z0-9_./-]+\.(?:json|lock|md|rs|toml|yaml|yml))"
)
referenced_inputs = set()
for relative in sorted(required_tools):
    text = (repo / relative).read_text(encoding="utf-8")
    referenced_inputs.update(repo_file.findall(text))

required_files = referenced | referenced_inputs | {"patches/transportd/tools/identity-lib.sh"}
for relative in sorted(required_files):
    path = repo / relative
    if not path.is_file():
        raise SystemExit(f"FAIL: referenced gate input is missing: {relative}")
    tracked = subprocess.run(
        ["git", "-C", str(repo), "ls-files", "--error-unmatch", relative],
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if tracked.returncode != 0:
        raise SystemExit(f"FAIL: referenced gate input is not tracked: {relative}")
    if path.suffix == ".sh" and not path.stat().st_mode & stat.S_IXUSR:
        raise SystemExit(f"FAIL: referenced script is not executable: {relative}")

worker = (repo / ".github/workflows/worker-ci.yml").read_text(encoding="utf-8")
release = (repo / ".github/workflows/transportd-release-gates.yml").read_text(encoding="utf-8")
runbook = (repo / "RUNBOOK.md").read_text(encoding="utf-8")
for name, text in {"worker-ci": worker, "release-gates": release, "RUNBOOK": runbook}.items():
    if "release-evidence-gate.sh --receipt" not in text:
        raise SystemExit(f"FAIL: {name} does not pass an explicit receipt path")
release_build = release.find("BUILD_PROFILE=release bash deploy/build-sidecar.sh prepare")
release_materialize = release.find("materialize-release-artifact.sh")
release_verify = release.find("release-tag-verify.sh", release_materialize)
if min(release_build, release_materialize, release_verify) < 0:
    raise SystemExit("FAIL: release workflow is missing build, materialize, or verify steps")
if not release_build < release_materialize < release_verify:
    raise SystemExit("FAIL: release workflow must build and materialize before verification")
for variable in {
    "CTD_RELEASE_ARTIFACT_PATH",
    "CTD_RELEASE_BINARY_PATH",
    "CTD_RELEASE_SBOM_PATH",
    "CTD_RELEASE_LICENSE_REPORT_PATH",
}:
    if release.count(variable) < 2:
        raise SystemExit(f"FAIL: release workflow does not carry {variable} into verification")
if "sidecar-image-receipt.txt" in "\n".join(path.read_text(encoding="utf-8") for path in inputs):
    raise SystemExit("FAIL: build/workflow documentation still names an in-tree receipt")


# gate 字段唯一性：sbom-gate.sh / wire-gate.sh 用 ctd_field（head -1）取值，
# 因此同一字段出现两次时"较旧的那组"会胜出，而闸门仍报绿——文件自相矛盾却无人发现。
# 这里在树级别兜底：每个字段至多一次（产品提交处这些字段本就不存在，故用 <=1）。
for _doc, _keys in (
    ("SBOM.md", ("sbom_source_commit", "sbom_source_tree", "sbom_cargo_lock_sha256",
                 "sbom_module_bazel_lock_sha256", "sbom_status", "sbom_artifact_status")),
    ("WIRE_COMPATIBILITY.md", ("wire_source_commit", "wire_source_tree", "wire_status")),
):
    _path = repo / _doc
    if not _path.is_file():
        continue
    _text = _path.read_text(encoding="utf-8")
    for _key in _keys:
        _n = len(re.findall(rf"^{_key}:", _text, re.M))
        if _n > 1:
            raise SystemExit(f"FAIL: {_doc} 中 {_key} 出现 {_n} 次（闸门取 head -1，会被旧值蒙混）")
print(
    f"PASS: {len(referenced)} script commands and {len(referenced_inputs)} "
    "non-shell gate inputs are tracked"
)
PY
