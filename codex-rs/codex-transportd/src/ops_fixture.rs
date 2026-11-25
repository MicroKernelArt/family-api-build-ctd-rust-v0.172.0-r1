//! 跨仓 fixture 解析（R2-codex）：显式集成清单 + 环境提供的仓库根。
//!
//! 契约（fail closed）：
//! - 清单路径只来自环境变量（调用方/CI 显式提供），绝不搜索当前目录；
//! - 清单内的 fixture 路径必须是仓库相对路径（拒绝绝对路径与 `..`）；
//! - 先校验身份（仓库 HEAD == 清单声明 commit），再读取内容；
//! - 任何缺失/不可读/身份不符都以可操作的错误失败，绝无静默回退。

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// 集成清单路径环境变量（调用方显式提供；无默认值）。
pub const FIXTURE_MANIFEST_ENV: &str = "CTD_OPS_FIXTURE_MANIFEST";
/// 跨仓 checkout 根环境变量（如 family-api-ops 工作树绝对路径）。
pub const OPS_ROOT_ENV: &str = "CTD_OPS_ROOT";
/// ops contract v1 封闭 operation 清单的角色名。
pub const ROLE_OPS_CONTRACT_INVENTORY: &str = "ops-contract-inventory";

/// 已通过身份校验的 fixture。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsFixture {
    pub role: String,
    /// fixture 绝对路径（<root>/<repo_relative_path>）。
    pub path: PathBuf,
    /// 校验通过的仓库 HEAD（== 清单 expected_commit）。
    pub commit: String,
}

/// 集成清单（repo 内版本化文件；机器可读）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct FixtureManifest {
    pub role: String,
    pub repo_relative_path: String,
    pub expected_commit: String,
}

/// 从环境变量解析（薄封装；缺失环境变量 = fail closed）。
pub fn resolve_from_env(role: &str) -> Result<OpsFixture, String> {
    let manifest_path = std::env::var_os(FIXTURE_MANIFEST_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "ops fixture 上下文缺失（fail closed）: set {FIXTURE_MANIFEST_ENV}=<integration manifest path>; \
                 no cwd/absolute fallback is performed"
            )
        })?;
    let root = std::env::var_os(OPS_ROOT_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| {
            format!(
                "ops fixture 上下文缺失（fail closed）: set {OPS_ROOT_ENV}=<cross-repo checkout root>"
            )
        })?;
    resolve_explicit(role, &manifest_path, &root)
}

/// 显式解析：清单文件 + 仓库根（身份校验先于任何内容读取）。
pub fn resolve_explicit(
    role: &str,
    manifest_path: &Path,
    root: &Path,
) -> Result<OpsFixture, String> {
    let raw = std::fs::read_to_string(manifest_path).map_err(|e| {
        format!(
            "cannot read integration manifest {}: {e}",
            manifest_path.display()
        )
    })?;
    let manifest: FixtureManifest = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "invalid integration manifest {}: {e}",
            manifest_path.display()
        )
    })?;
    if manifest.role != role {
        return Err(format!(
            "integration manifest role mismatch: manifest declares '{}', requested '{role}'",
            manifest.role
        ));
    }
    let rel = validate_relative_path(&manifest.repo_relative_path)?;
    if !root.is_absolute() || !root.is_dir() {
        return Err(format!(
            "ops root {} is not an absolute directory",
            root.display()
        ));
    }
    let commit = git_head(root)?;
    if commit != manifest.expected_commit {
        return Err(format!(
            "ops fixture identity mismatch: {} HEAD {commit} != manifest expected {}",
            root.display(),
            manifest.expected_commit
        ));
    }
    let path = root.join(rel);
    let meta = std::fs::metadata(&path)
        .map_err(|e| format!("ops fixture {} unavailable: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "ops fixture {} is not a regular file",
            path.display()
        ));
    }
    Ok(OpsFixture {
        role: manifest.role,
        path,
        commit,
    })
}

/// 拒绝绝对路径、`..` 组件与空路径（只允许仓库相对安全路径）。
fn validate_relative_path(rel: &str) -> Result<&str, String> {
    if rel.is_empty() {
        return Err("integration manifest repo_relative_path is empty".to_string());
    }
    let candidate = Path::new(rel);
    if candidate.is_absolute()
        || candidate
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!(
            "integration manifest repo_relative_path must be a plain repo-relative path: '{rel}'"
        ));
    }
    Ok(rel)
}

/// 仓库 HEAD（`git rev-parse HEAD`）；非 git/失败 = fail closed。
fn git_head(root: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot determine HEAD of {}: {e}", root.display()))?;
    if !output.status.success() {
        return Err(format!(
            "cannot determine HEAD of {} (not a git checkout?): {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if commit.len() != 40 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("unexpected git HEAD format: '{commit}'"));
    }
    Ok(commit)
}

#[cfg(test)]
#[path = "ops_fixture_tests.rs"]
mod tests;
