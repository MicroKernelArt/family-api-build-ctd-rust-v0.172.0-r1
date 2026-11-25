//! installation bootstrap：只通过官方 `codex_core::resolve_installation_id`
//! 写入/读取 `CODEX_HOME/installation_id`，不实现第二写入者。

use std::path::Path;

use codex_utils_absolute_path::AbsolutePathBuf;

/// 使用官方 resolver 解析（必要时生成并持久化）installation ID。
pub async fn ensure_installation_id(codex_home: &Path) -> Result<String, String> {
    let home = AbsolutePathBuf::from_absolute_path(codex_home)
        .map_err(|e| format!("invalid codex_home {}: {e}", codex_home.display()))?;
    let id = codex_core::resolve_installation_id(&home)
        .await
        .map_err(|e| format!("official resolve_installation_id failed: {e}"))?;
    tracing::info!(codex_home = %codex_home.display(), "installation id resolved (bootstrap ok)");
    Ok(id)
}
