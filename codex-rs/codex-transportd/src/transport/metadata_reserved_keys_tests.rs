//! R-METADATA-001（replay_0831 R6）：sidecar 拒绝每一个 core 保留
//! `client_metadata` 键。
//!
//! 深度 parity：sidecar 校验直接采用 `codex_core::RESERVED_METADATA_KEYS`
//! 单一事实来源；本测试镜像逐键行为断言——列表相等性 + 每一个键在
//! `extract_client_metadata` 处 fail closed。校验发生在任何 upstream I/O
//! 之前（纯函数，无 client/pool 形参；端到端零连接证明见
//! `tests/upstream_non2xx_preservation.rs` 的 reserved_key 场景）。

use super::extract_client_metadata;
use super::validate_body_object;

// 0117：transportd 以别名直连 `codex_core::RESERVED_METADATA_KEYS`（无镜像拷贝）；
// 本测试只做逐键 parity 与 fail-closed 断言。

#[test]
fn sidecar_list_is_identical_to_core_source_of_truth() {
    // 0117 单一事实来源契约：transportd 的 strip 直接别名
    // `codex_core::RESERVED_METADATA_KEYS`（transport/metadata.rs），
    // 不做本地镜像拷贝，杜绝 0.159.0 后新增保留键漏剥。
    // 上游 BACKWARD_COMPATIBLE_RESERVED_METADATA_KEYS 仅放宽 core 自身对
    // 两个 legacy 键的配置校验；sidecar 出站 strip 仍按完整 RESERVED 集
    // 剥离（fail-closed），不受该兼容层影响。
    let mut core: Vec<&str> = codex_core::RESERVED_METADATA_KEYS.to_vec();
    let mut mirrored: Vec<&str> = crate::transport::metadata::RESERVED_METADATA_KEYS.to_vec();
    core.sort_unstable();
    mirrored.sort_unstable();
    assert_eq!(core, mirrored, "sidecar 必须与 core 保留键列表逐键一致");
    let unique = core.len();
    core.dedup();
    assert_eq!(core.len(), unique, "core 保留键列表不得有重复项");
}

#[test]
fn every_core_reserved_key_is_rejected_before_io() {
    for key in codex_core::RESERVED_METADATA_KEYS {
        let mut client_metadata = serde_json::Map::new();
        client_metadata.insert((*key).to_string(), serde_json::json!("attacker-controlled"));
        let body = serde_json::json!({
            "model": "gpt-5.2",
            "client_metadata": client_metadata,
        });
        validate_body_object(&body).expect("root 层无保留键冲突");
        let err = match extract_client_metadata(body.as_object().expect("object")) {
            Ok(extra) => panic!("core 保留键 {key} 必须在 upstream I/O 前被拒绝: {extra:?}"),
            Err(err) => err,
        };
        assert_eq!(err.code, "reserved_metadata_conflict", "key {key}");
    }
}

#[test]
fn parent_thread_id_payload_is_rejected() {
    // R6 审查的具体 payload：caller 不得主张官方 parent-thread 血缘。
    let body = serde_json::json!({
        "model": "gpt-5.2",
        "client_metadata": {"x-codex-parent-thread-id": "attacker"},
    });
    let err = extract_client_metadata(body.as_object().expect("object"))
        .expect_err("x-codex-parent-thread-id 必须被拒绝");
    assert_eq!(err.code, "reserved_metadata_conflict");
    assert!(err.message.contains("x-codex-parent-thread-id"));
}

#[test]
fn non_reserved_keys_still_pass_through() {
    // 不弱化允许面：非保留 extra 键仍无损放行。
    let body = serde_json::json!({
        "client_metadata": {"custom_key": "v", "team": {"nested": true}},
    });
    let extra = extract_client_metadata(body.as_object().expect("object")).expect("extra");
    assert_eq!(extra.len(), 2);
}
