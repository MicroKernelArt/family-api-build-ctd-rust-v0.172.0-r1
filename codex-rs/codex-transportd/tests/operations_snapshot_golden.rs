//! operations 快照 golden 测试（R3-codex / SCN-006）。
//!
//! 渲染输出必须与入库 golden 逐字节一致（确定性排序 + 稳定序列化）。
//! operation 集合任何变化（缺失/新增/改名/字段漂移）都会使本测试失败，
//! 由此强制 golden 随产品契约同步评审。再生成方式：
//!   CTD_SNAPSHOT_WRITE=1 cargo test -p codex-transportd --test operations_snapshot_golden
//! （写回 golden 后仍执行一致性断言；diff 随实现一并提交。）

use std::path::PathBuf;

const GOLDEN_RELPATH: &str = codex_transportd::contract::operations_snapshot::GOLDEN_RELPATH;

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDEN_RELPATH)
}

#[test]
fn operations_snapshot_matches_checked_in_golden() {
    let rendered = codex_transportd::contract::operations_snapshot::render_snapshot();

    // 再生成模式：显式环境变量才允许写回（默认只读，测试绝不静默改库）。
    if std::env::var_os("CTD_SNAPSHOT_WRITE").is_some_and(|v| v == "1") {
        let path = golden_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixtures dir");
        }
        std::fs::write(&path, &rendered).expect("write golden");
        println!("golden rewritten: {}", path.display());
    }

    let golden = std::fs::read_to_string(golden_path())
        .expect("golden file missing — run once with CTD_SNAPSHOT_WRITE=1 and commit the result");
    assert_eq!(
        rendered, golden,
        "operations snapshot drifted from checked-in golden \
         (missing/extra/renamed operation?); re-run with CTD_SNAPSHOT_WRITE=1 and review the diff"
    );
}

#[test]
fn golden_matches_manifest_declared_operations() {
    // 独立于渲染的最小一致性断言：golden 覆盖 ALL 表中的每个 operation。
    let golden = std::fs::read_to_string(golden_path()).expect("golden file");
    let parsed: serde_json::Value = serde_json::from_str(&golden).expect("golden json");
    let listed: Vec<&str> = parsed["operations"]
        .as_array()
        .expect("operations array")
        .iter()
        .map(|r| r["operation"].as_str().expect("operation label"))
        .collect();
    assert_eq!(
        listed.len(),
        codex_transportd::contract::operation::Operation::ALL.len(),
        "golden row count must equal Operation::ALL (no hand-maintained list)"
    );
}
