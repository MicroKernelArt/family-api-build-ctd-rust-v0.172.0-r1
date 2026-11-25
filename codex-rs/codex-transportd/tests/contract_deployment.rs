//! R1-codex 部署契约测试：CLI/config 面、health/info 路由、所有权负向保证。
//!
//! 契约（closure matrix R1-codex）：
//! - CLI/health 契约稳定且可测试（clean Compose 可直接使用）；
//! - 文件系统属主归部署方（如 UID 10001）——daemon 只验证，
//!   绝不 chmod/chown/delete 调用方资源；
//! - 不可写路径/属主不符必须以可操作错误快速失败。

use std::io::Read;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;
use std::time::Instant;

use codex_transportd::config::Config;

fn parse_args(args: &[&str]) -> Result<Config, String> {
    Config::parse_from(args.iter().map(|s| (*s).to_string()))
}

fn socket_arg(dir: &Path) -> String {
    dir.join("t.sock").display().to_string()
}

// ---------------------------------------------------------------- CLI 契约

#[test]
fn cli_requires_socket() {
    let err = parse_args(&[]).expect_err("--socket is required");
    assert!(err.contains("--socket is required"), "unexpected: {err}");
}

#[test]
fn cli_accepts_documented_flag_set() {
    let cfg = parse_args(&[
        "--socket",
        "/run/ctd/t.sock",
        "--codex-home",
        "/var/lib/ctd/home",
        "--image-digest",
        "sha256:a",
        "--build-proof-digest",
        "sha256:b",
        "--socket-dir-uid",
        "10001",
        "--socket-dir-gid",
        "2000",
    ])
    .expect("documented flags must parse");
    assert_eq!(cfg.socket_path, Path::new("/run/ctd/t.sock"));
    assert_eq!(cfg.codex_home, Path::new("/var/lib/ctd/home"));
    assert_eq!(cfg.image_digest_override.as_deref(), Some("sha256:a"));
    assert_eq!(cfg.build_proof_digest_override.as_deref(), Some("sha256:b"));
    assert_eq!(cfg.socket_dir_expected_uid, Some(10001));
    assert_eq!(cfg.socket_dir_expected_gid, Some(2000));
}

#[test]
fn cli_rejects_unknown_flag() {
    let err = parse_args(&["--socket", "/tmp/s.sock", "--bogus"]).expect_err("unknown flag");
    assert!(err.contains("unknown argument"), "unexpected: {err}");
}

#[test]
fn cli_rejects_missing_flag_value() {
    for flag in ["--socket", "--codex-home", "--socket-dir-uid"] {
        let err = parse_args(&[flag]).expect_err("missing value");
        assert!(
            err.contains("requires a value"),
            "flag {flag}: unexpected: {err}"
        );
    }
}

#[test]
fn cli_rejects_malformed_uid_gid() {
    let err = parse_args(&["--socket", "/tmp/s.sock", "--socket-dir-uid", "10001x"])
        .expect_err("bad uid");
    assert!(
        err.contains("invalid --socket-dir-uid"),
        "unexpected: {err}"
    );
    let err =
        parse_args(&["--socket", "/tmp/s.sock", "--socket-dir-gid", "-1"]).expect_err("bad gid");
    assert!(
        err.contains("invalid --socket-dir-gid"),
        "unexpected: {err}"
    );
}

#[test]
fn help_reports_usage_without_serving() {
    let err = parse_args(&["--help"]).expect_err("help is reported via Err(\"help\")");
    assert_eq!(err, "help");
}

// ------------------------------------------------------------- server 契约

fn secure_socket_dir(base: &Path) -> std::path::PathBuf {
    let dir = base.join("sock");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).expect("chmod 0770");
    dir
}

fn test_config(socket_dir: &Path) -> Config {
    parse_args(&[
        "--socket",
        &socket_arg(socket_dir),
        "--codex-home",
        &std::env::temp_dir()
            .join(format!("ctd-home-{}", std::process::id()))
            .display()
            .to_string(),
    ])
    .expect("valid config")
}

/// 阻塞式 UDS HTTP 交换（move 进 spawn_blocking 前不可借用临时路径）。
fn http_over_uds_blocking(
    path: String,
    socket: std::path::PathBuf,
) -> Result<(u16, String), String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(&socket).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&buf).to_string();
    let status: u16 = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("malformed response: {text}"))?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    Ok((status, body))
}

/// RUST-HEALTH-002：health/info 契约交换必须经 spawn_blocking。
///
/// 根因（hosted run 33508194105，`Resource temporarily unavailable
/// (os error 11)`）：`#[tokio::test]` 默认 current_thread runtime，
/// 测试线程里对 spawn 出的 `serve` 做阻塞式 std UnixStream 读写会占住
/// runtime 唯一线程，hyper accept/respond 任务无法被 poll，5s 读超时
/// （SO_RCVTIMEO）后 read 返回 EAGAIN。把阻塞交换移入 blocking pool，
/// runtime 线程保持空闲可调度 server；每次交换外加有界总超时，
/// 不引入无界 retry/sleep，也不放宽任何断言。
async fn http_over_uds(path: &str, socket: &Path) -> Result<(u16, String), String> {
    let path = path.to_string();
    let socket = socket.to_path_buf();
    let handle = tokio::task::spawn_blocking(move || http_over_uds_blocking(path, socket));
    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .map_err(|_| "internal route exchange timed out".to_string())?
        .map_err(|join| format!("internal route exchange panicked: {join}"))?
}

#[tokio::test]
async fn health_and_info_contract_on_internal_routes() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = secure_socket_dir(tmp.path());
    let socket = dir.join("t.sock");

    let server = tokio::spawn(codex_transportd::server::serve(test_config(&dir)));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "server did not bind socket in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 契约路由（server/mod.rs router）：health/live、health/ready、info。
    let (status, body) = http_over_uds("/_internal/v1/health/live", &socket)
        .await
        .expect("live");
    assert_eq!(status, 200);
    assert_eq!(body, "ok");
    let (status, body) = http_over_uds("/_internal/v1/health/ready", &socket)
        .await
        .expect("ready");
    assert_eq!(status, 200);
    assert_eq!(body, "ok");
    let (status, body) = http_over_uds("/_internal/v1/info", &socket)
        .await
        .expect("info");
    assert_eq!(status, 200);
    let info: serde_json::Value = serde_json::from_str(&body).expect("info json");
    assert!(
        info.get("fork_commit").is_some() && info.get("target").is_some(),
        "info must expose fork identity: {info}"
    );
    // 契约外路径拒绝：README 旧文档路径 /health/* 不存在（防回归）。
    let (status, _) = http_over_uds("/health/ready", &socket)
        .await
        .expect("legacy path");
    assert_eq!(status, 404, "legacy /health/* must not be served");
    server.abort();
}

// ----------------------------------------------------- 所有权负向（PS-M4）

fn metadata_of(path: &Path) -> (std::fs::Metadata, u64) {
    let meta = std::fs::symlink_metadata(path).expect("metadata");
    let mode = meta.permissions().mode();
    (meta, u64::from(mode))
}

#[tokio::test]
async fn existing_regular_file_at_socket_path_is_never_removed_or_modified() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = secure_socket_dir(tmp.path());
    let victim = dir.join("t.sock");
    std::fs::write(&victim, b"caller-owned data").expect("write victim");
    let (before, mode_before) = metadata_of(&victim);

    let err = tokio::time::timeout(
        Duration::from_secs(10),
        codex_transportd::server::serve(test_config(&dir)),
    )
    .await
    .expect("serve must fail fast, not serve")
    .expect_err("regular file at socket path must be refused");
    assert!(
        err.contains("refusing to remove it"),
        "unexpected error: {err}"
    );

    let (after, mode_after) = metadata_of(&victim);
    assert_eq!(after.len(), before.len(), "file content must be untouched");
    assert_eq!(mode_before, mode_after, "file mode must be untouched");
    assert_eq!(
        std::fs::read(&victim).expect("read victim"),
        b"caller-owned data",
        "file bytes must be untouched"
    );
}

#[tokio::test]
async fn symlink_at_socket_path_is_refused_and_preserved() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = secure_socket_dir(tmp.path());
    let target = tmp.path().join("target.txt");
    std::fs::write(&target, b"target").expect("write target");
    let link = dir.join("t.sock");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    let err = tokio::time::timeout(
        Duration::from_secs(10),
        codex_transportd::server::serve(test_config(&dir)),
    )
    .await
    .expect("serve must fail fast")
    .expect_err("symlink socket path must be refused");
    assert!(err.contains("symlink"), "unexpected error: {err}");
    assert!(
        std::fs::symlink_metadata(&link).is_ok(),
        "symlink must not be removed"
    );
    assert_eq!(std::fs::read(&target).expect("target"), b"target");
}

#[tokio::test]
async fn world_writable_socket_dir_is_refused_without_chmod() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = tmp.path().join("sock");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod 0777");

    let err = tokio::time::timeout(
        Duration::from_secs(10),
        codex_transportd::server::serve(test_config(&dir)),
    )
    .await
    .expect("serve must fail fast")
    .expect_err("world-writable socket dir must be refused");
    assert!(err.contains("world-writable"), "unexpected error: {err}");
    let (_, mode_after) = metadata_of(&dir);
    assert_eq!(
        mode_after & 0o777,
        0o777,
        "daemon must not chmod the deployment-owned directory"
    );
}

#[tokio::test]
async fn owner_uid_mismatch_is_refused_without_chown() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = secure_socket_dir(tmp.path());
    use std::os::unix::fs::MetadataExt;
    let actual_uid = std::fs::metadata(&dir).expect("stat").uid();

    let mut cfg = test_config(&dir);
    cfg.socket_dir_expected_uid = Some(actual_uid.wrapping_add(1));

    let err = tokio::time::timeout(
        Duration::from_secs(10),
        codex_transportd::server::serve(cfg),
    )
    .await
    .expect("serve must fail fast")
    .expect_err("uid mismatch must be refused");
    assert!(err.contains("owner uid"), "unexpected error: {err}");
    assert_eq!(
        std::fs::metadata(&dir).expect("stat").uid(),
        actual_uid,
        "daemon must not chown the deployment-owned directory"
    );
}

#[tokio::test]
async fn unwritable_socket_dir_fails_with_actionable_error_and_no_deletion() {
    // RUST-DEPLOY-001（cleanup ownership）：失败路径下 daemon 不得删除或
    // 改写调用方预存资源，也不得 chmod/chown 部署方目录。修复前的测试把
    // marker 写在 chmod 0555 之后（非 root 下写入被吞、marker 从未存在，
    // 断言必然失败）；正确设计是先把 marker 放进可写目录，再锁目录为
    // 0555——daemon 若在失败路径尝试任何 chmod/删除都会破坏断言。
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dir = tmp.path().join("sock");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).expect("chmod 0770");
    let marker = dir.join("deployment-marker");
    std::fs::write(&marker, b"keep").expect("write marker before locking the dir down");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod 0555");
    let probe = dir.join("write-probe");
    if std::fs::write(&probe, b"x").is_ok() {
        // 以 root 运行时 0555 目录仍可写（无法构造不可写场景）——跳过。
        eprintln!("running with elevated privileges; skipping unwritable-dir scenario");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770))
            .expect("restore mode");
        return;
    }
    let err = tokio::time::timeout(
        Duration::from_secs(10),
        codex_transportd::server::serve(test_config(&dir)),
    )
    .await
    .expect("serve must fail fast")
    .expect_err("unwritable socket dir must fail with actionable error");
    assert!(
        err.contains("cannot open socket lock") || err.contains("cannot create socket dir"),
        "unexpected error: {err}"
    );
    // daemon 不得为了"清理/继续"而把部署方目录改回可写：失败返回时
    // 目录必须仍是 0555（root 之外，这同时保证 marker 不可能被删除）。
    let (_, mode_after) = metadata_of(&dir);
    assert_eq!(
        mode_after & 0o777,
        0o555,
        "daemon must not chmod the deployment-owned directory"
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).expect("restore mode");
    assert_eq!(
        std::fs::read(&marker).expect("marker must survive the failed startup"),
        b"keep",
        "deployment marker must not be deleted or modified"
    );
}
