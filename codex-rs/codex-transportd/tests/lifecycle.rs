//! installation 生命周期矩阵（CP-002 gate）：
//! 进程重启、同 volume 容器重建、新空 volume、备份/恢复、注入尝试。
//! 全部只通过官方 `codex_core::resolve_installation_id` 操作。

use codex_core::resolve_installation_id;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::fs;
use std::path::Path;

fn abs(path: &Path) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(path).expect("absolute path")
}

fn write_home_file(home: &Path, content: &str) {
    fs::create_dir_all(home).expect("create home");
    fs::write(home.join("installation_id"), content).expect("write installation_id");
}

#[tokio::test]
async fn fresh_bootstrap_creates_uuid() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let id = resolve_installation_id(&abs(home.path()))
        .await
        .expect("resolve");
    assert!(uuid::Uuid::parse_str(&id).is_ok());
    let persisted = fs::read_to_string(home.path().join("installation_id")).expect("read");
    assert_eq!(persisted, id);
}

#[tokio::test]
async fn process_restart_keeps_same_id() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let first = resolve_installation_id(&abs(home.path()))
        .await
        .expect("first");
    let second = resolve_installation_id(&abs(home.path()))
        .await
        .expect("second");
    assert_eq!(first, second);
}

#[tokio::test]
async fn same_volume_container_rebuild_keeps_id() {
    // 容器重建 = 同一 CODEX_HOME volume 再次挂载并重新运行 resolver。
    let volume = tempfile::TempDir::new().expect("tempdir");
    let before = resolve_installation_id(&abs(volume.path()))
        .await
        .expect("before");
    // 模拟重建：删除进程产物（无），volume 不变，再次解析。
    let after = resolve_installation_id(&abs(volume.path()))
        .await
        .expect("after");
    assert_eq!(before, after);
}

#[tokio::test]
async fn new_empty_volume_gets_new_id() {
    let v1 = tempfile::TempDir::new().expect("v1");
    let v2 = tempfile::TempDir::new().expect("v2");
    let id1 = resolve_installation_id(&abs(v1.path())).await.expect("id1");
    let id2 = resolve_installation_id(&abs(v2.path())).await.expect("id2");
    assert_ne!(id1, id2);
}

#[tokio::test]
async fn backup_restore_keeps_id() {
    let home = tempfile::TempDir::new().expect("tempdir");
    let id = resolve_installation_id(&abs(home.path()))
        .await
        .expect("id");
    let backup = home.path().join("installation_id.backup");
    fs::copy(home.path().join("installation_id"), &backup).expect("backup");
    // 模拟故障后从备份恢复。
    fs::copy(&backup, home.path().join("installation_id")).expect("restore");
    let restored = resolve_installation_id(&abs(home.path()))
        .await
        .expect("restored");
    assert_eq!(id, restored);
}

#[tokio::test]
async fn invalid_injection_is_rejected_and_rewritten() {
    let home = tempfile::TempDir::new().expect("tempdir");
    write_home_file(home.path(), "injected-not-a-uuid");
    let id = resolve_installation_id(&abs(home.path()))
        .await
        .expect("resolve");
    assert!(uuid::Uuid::parse_str(&id).is_ok());
    let persisted = fs::read_to_string(home.path().join("installation_id")).expect("read");
    assert_eq!(persisted, id);
    assert_ne!(persisted, "injected-not-a-uuid");
}

#[tokio::test]
async fn mode_is_0644() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::TempDir::new().expect("tempdir");
        let _ = resolve_installation_id(&abs(home.path()))
            .await
            .expect("resolve");
        let mode = fs::metadata(home.path().join("installation_id"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);
    }
}

// F-017：socket 单实例锁——活动实例存在时第二个 serve 必须失败。
#[tokio::test]
async fn socket_singleton_lock_rejects_second_instance() {
    use codex_transportd::config::Config;
    use codex_transportd::server::serve;

    let dir = tempfile::TempDir::new().expect("tempdir");
    let cfg = Config {
        codex_home: dir.path().join("home"),
        socket_path: dir.path().join("s.sock"),
        image_digest_override: None,
        build_proof_digest_override: None,
        socket_dir_expected_uid: None,
        socket_dir_expected_gid: None,
    };
    let first = tokio::spawn(serve(cfg.clone()));
    // 等待 socket 就绪。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if dir.path().join("s.sock").exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "first instance did not bind"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 第二个实例：锁冲突 → 启动失败。
    let second = serve(cfg).await;
    assert!(second.is_err(), "second instance must fail to start");

    // 清理。
    first.abort();
    let _ = first.await;
}
