//! 构建脚本：注入编译期 release 证据（F-005）。
//!
//! - CODEX_TRANSPORTD_TARGET: cargo TARGET 三元组
//! - CODEX_TRANSPORTD_FORK_COMMIT: fork 仓库 HEAD（构建时 git 解析；
//!   非 git 检出/解析失败时为空，由 /info 校验失败暴露）
//! - CODEX_TRANSPORTD_FORK_LOCK: codex-rs/Cargo.lock SHA-256
//! - CODEX_TRANSPORTD_BUILD_PROOF_DIGEST: 外部注入的 binary digest
//!   （CI 构建后计算；本地构建为空，/info build_proof_digest 显示空）

use sha2::Digest;
use sha2::Sha256;
use std::path::Path;
use std::process::Command;

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=CODEX_TRANSPORTD_TARGET={target}");

    // fork HEAD（构建时解析，禁止 CLI 参数伪造）。
    // CI 容器没有 .git，由 build-sidecar.sh 显式注入不可变 source_commit。
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let repo_root = Path::new(&manifest_dir).join("..").join("..");
    let injected_commit = std::env::var("CODEX_TRANSPORTD_FORK_COMMIT")
        .ok()
        .filter(|value| !value.is_empty());
    if let Some(commit) = injected_commit {
        println!("cargo:rustc-env=CODEX_TRANSPORTD_FORK_COMMIT={commit}");
    } else {
        let git_output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo_root)
            .output();
        if let Ok(output) = git_output
            && output.status.success()
        {
            let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !commit.is_empty() {
                println!("cargo:rustc-env=CODEX_TRANSPORTD_FORK_COMMIT={commit}");
            }
        }
    }

    // codex-rs/Cargo.lock SHA-256（构建时计算）。
    let lock_path = repo_root.join("codex-rs").join("Cargo.lock");
    if let Ok(lock_bytes) = std::fs::read(&lock_path) {
        let mut hasher = Sha256::new();
        hasher.update(&lock_bytes);
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(64);
        for byte in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{byte:02x}");
        }
        println!("cargo:rustc-env=CODEX_TRANSPORTD_FORK_LOCK={hex}");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../codex-rs/Cargo.lock");
    // P0-2/PS-M5/PSR2-H07：HEAD/ref 变化必须触发 build.rs 重跑——否则
    // fork_commit 跨重建静默停留旧值（承重 release 证据）。
    // 通过 git rev-parse --git-path 解析实际 gitdir（覆盖普通 clone /
    // linked worktree / detached HEAD），不拼接假设的 ../../.git。
    if let Some(head_path) = git_path(&repo_root, "HEAD") {
        println!("cargo:rerun-if-changed={}", head_path.display());
    }
    if let Some(refs_path) = git_path(&repo_root, "refs") {
        println!("cargo:rerun-if-changed={}", refs_path.display());
    }
    if let Some(packed_path) = git_path(&repo_root, "packed-refs") {
        println!("cargo:rerun-if-changed={}", packed_path.display());
    }
}

/// git_path 通过 `git rev-parse --git-path <name>` 解析实际 git 内部路径
/// （PSR2-H07）：linked worktree 的 gitdir 不在 repo_root/.git，拼接会
/// 指向不存在的路径，导致 rerun-if-changed 失效（fork 证据陈旧）。
fn git_path(repo_root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let p = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_string());
    Some(if p.is_absolute() {
        p
    } else {
        repo_root.join(p)
    })
}
