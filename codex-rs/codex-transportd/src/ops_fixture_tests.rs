//! `ops_fixture` 单元测试（R2-codex）：显式解析的失败封闭语义。
//!
//! 覆盖：正路径（真实 git 仓 + 相对路径 + 身份一致）、身份不符、绝对路径、
//! `..` 穿越、角色不符、fixture 缺失、非 git 根。缺环境变量的路径由
//! `resolve_from_env` 直接映射为错误（集成侧由 val006 `#[ignore]` 测试
//! 在三仓拓扑中执行）。

use std::path::Path;
use std::process::Command;

/// 在临时目录内构造真实 git 仓库（`git init` + 单 commit），返回 (root, HEAD)。
fn init_git_repo(dir: &Path, file_rel: &str, content: &str) -> String {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    run(&["init", "--quiet"]);
    run(&["config", "user.email", "test@example.invalid"]);
    run(&["config", "user.name", "fixture-test"]);
    let file = dir.join(file_rel);
    std::fs::create_dir_all(file.parent().unwrap()).expect("mkdir");
    std::fs::write(&file, content).expect("write fixture");
    run(&["add", "."]);
    run(&["commit", "--quiet", "-m", "fixture"]);
    let head = run(&["rev-parse", "HEAD"]);
    String::from_utf8_lossy(&head.stdout).trim().to_string()
}

fn write_manifest(dir: &Path, role: &str, rel: &str, commit: &str) -> std::path::PathBuf {
    let path = dir.join("manifest.json");
    std::fs::write(
        &path,
        format!(r#"{{"role":"{role}","repo_relative_path":"{rel}","expected_commit":"{commit}"}}"#),
    )
    .expect("write manifest");
    path
}

#[test]
fn positive_resolves_relative_fixture_with_matching_identity() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let commit = init_git_repo(tmp.path(), "feature/tls/contract/operations.json", "{}");
    let manifest = write_manifest(
        tmp.path(),
        "ops-contract-inventory",
        "feature/tls/contract/operations.json",
        &commit,
    );
    let fixture = codex_transportd::ops_fixture::resolve_explicit(
        "ops-contract-inventory",
        &manifest,
        tmp.path(),
    )
    .expect("resolve");
    assert_eq!(fixture.commit, commit);
    assert_eq!(fixture.role, "ops-contract-inventory");
    assert_eq!(
        fixture.path,
        tmp.path().join("feature/tls/contract/operations.json")
    );
}

#[test]
fn identity_mismatch_fails_closed() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let _commit = init_git_repo(tmp.path(), "ops.json", "{}");
    let manifest = write_manifest(
        tmp.path(),
        "ops-contract-inventory",
        "ops.json",
        "1234567890123456789012345678901234567890",
    );
    let err = codex_transportd::ops_fixture::resolve_explicit(
        "ops-contract-inventory",
        &manifest,
        tmp.path(),
    )
    .expect_err("identity mismatch must fail");
    assert!(err.contains("identity mismatch"), "unexpected error: {err}");
}

#[test]
fn absolute_and_traversal_paths_are_rejected() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let commit = init_git_repo(tmp.path(), "ops.json", "{}");
    for rel in ["/etc/passwd", "../outside.json", "a/../../b.json", ""] {
        let manifest = write_manifest(tmp.path(), "ops-contract-inventory", rel, &commit);
        let err = codex_transportd::ops_fixture::resolve_explicit(
            "ops-contract-inventory",
            &manifest,
            tmp.path(),
        )
        .expect_err("unsafe path must fail");
        assert!(
            err.contains("repo-relative") || err.contains("empty"),
            "unexpected error for '{rel}': {err}"
        );
    }
}

#[test]
fn role_mismatch_fails_closed() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let commit = init_git_repo(tmp.path(), "ops.json", "{}");
    let manifest = write_manifest(tmp.path(), "ops-contract-inventory", "ops.json", &commit);
    let err = codex_transportd::ops_fixture::resolve_explicit("other-role", &manifest, tmp.path())
        .expect_err("role mismatch must fail");
    assert!(err.contains("role mismatch"), "unexpected error: {err}");
}

#[test]
fn missing_fixture_file_fails_closed() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let commit = init_git_repo(tmp.path(), "present.json", "{}");
    let manifest = write_manifest(tmp.path(), "ops-contract-inventory", "absent.json", &commit);
    let err = codex_transportd::ops_fixture::resolve_explicit(
        "ops-contract-inventory",
        &manifest,
        tmp.path(),
    )
    .expect_err("missing fixture must fail");
    assert!(err.contains("unavailable"), "unexpected error: {err}");
}

#[test]
fn non_git_root_fails_closed() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let empty = tempfile::TempDir::new().expect("tempdir");
    let manifest = write_manifest(
        tmp.path(),
        "ops-contract-inventory",
        "ops.json",
        &"0".repeat(40),
    );
    let err = codex_transportd::ops_fixture::resolve_explicit(
        "ops-contract-inventory",
        &manifest,
        empty.path(),
    )
    .expect_err("non-git root must fail");
    assert!(
        err.contains("cannot determine HEAD"),
        "unexpected error: {err}"
    );
}

#[test]
fn relative_root_is_rejected() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let commit = init_git_repo(tmp.path(), "ops.json", "{}");
    let manifest = write_manifest(tmp.path(), "ops-contract-inventory", "ops.json", &commit);
    let err = codex_transportd::ops_fixture::resolve_explicit(
        "ops-contract-inventory",
        &manifest,
        Path::new("relative-root"),
    )
    .expect_err("relative root must fail");
    assert!(
        err.contains("absolute directory"),
        "unexpected error: {err}"
    );
}
