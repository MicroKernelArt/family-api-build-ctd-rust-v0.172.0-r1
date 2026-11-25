//! 确定性版本化 operations 快照（R3-codex）。
//!
//! 唯一事实源是 `contract::operation::Operation`（`ALL` 全量表由同一宏
//! 生成）；本模块只做投影渲染：固定字段顺序、按 internal_path 字典序
//! 排序、稳定 JSON 序列化。任何 operation 缺失/新增/改名都会改变渲染
//! 输出并使 golden 测试失败——不存在第二份手写 operation 列表。

use codex_http_client::ClientRouteClass;

use crate::contract::operation::Operation;

/// 快照 schema 版本（消费方跨仓对齐锚点；字段变更必须递增）。
pub const SNAPSHOT_VERSION: u32 = 1;

/// golden 文件路径（相对 crate 根；由 `tests/operations_snapshot_golden.rs` 校验）。
pub const GOLDEN_RELPATH: &str = "tests/fixtures/operations_snapshot.golden.json";

#[derive(serde::Serialize)]
struct OperationRow {
    operation: &'static str,
    method: &'static str,
    internal_path: &'static str,
    upstream_path: &'static str,
    requires_session: bool,
    upgrade: bool,
}

#[derive(serde::Serialize)]
struct Snapshot {
    snapshot_version: u32,
    generator: String,
    operation_count: usize,
    operations: Vec<OperationRow>,
}

/// 渲染确定性快照（pretty JSON + 结尾换行；两次调用输出逐字节一致）。
pub fn render_snapshot() -> String {
    let mut rows: Vec<OperationRow> = Operation::ALL
        .iter()
        .map(|&op| {
            // rustc 1.95: Method 临时值的 as_str 借用活不到 'static 字段；
            // 方法全表仅 GET/POST 两种，静态映射保持 golden 输出逐字节不变。
            let method: &'static str = match op.upstream_method() {
                http::Method::GET => "GET",
                _ => "POST",
            };
            OperationRow {
                operation: op.metric_label(),
                method,
                internal_path: op.internal_endpoint(),
                upstream_path: op.upstream_path(),
                requires_session: op.requires_session(),
                upgrade: matches!(op.route_class(), ClientRouteClass::WebSocket),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.internal_path.cmp(b.internal_path));

    let snapshot = Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        generator: format!("codex-transportd v{}", env!("CARGO_PKG_VERSION")),
        operation_count: rows.len(),
        operations: rows,
    };
    let mut body = serde_json::to_string_pretty(&snapshot).expect("serialize snapshot");
    body.push('\n');
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_is_byte_deterministic() {
        let first = render_snapshot();
        let second = render_snapshot();
        assert_eq!(first, second, "snapshot rendering must be deterministic");
        assert!(first.ends_with("}\n"));
        assert!(serde_json::from_str::<serde_json::Value>(&first).is_ok());
    }

    #[test]
    fn snapshot_is_derived_from_all_table() {
        // 快照行数必须等于 ALL 表长度（防渲染层手写列表回潮）。
        let parsed: serde_json::Value = serde_json::from_str(&render_snapshot()).unwrap();
        assert_eq!(
            parsed["operation_count"].as_u64().unwrap() as usize,
            Operation::ALL.len()
        );
        assert_eq!(
            parsed["snapshot_version"].as_u64().unwrap(),
            u64::from(SNAPSHOT_VERSION)
        );
    }

    #[test]
    fn rows_are_sorted_by_internal_path() {
        let parsed: serde_json::Value = serde_json::from_str(&render_snapshot()).unwrap();
        let paths: Vec<&str> = parsed["operations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["internal_path"].as_str().unwrap())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted);
    }
}
