//! Agent 身份平面(P1-D / OP-019)。
//!
//! agent task register 与官方 codex-rs `register_agent_task` 签名同构
//! (ed25519 + `{runtime_id}:{timestamp}` + PKCS8 base64 + 固定端点
//! `/v1/agent/{runtime_id}/task/register`)。sidecar 直接复用官方组件:
//! 私钥经 X-CTD-Agent-Private-Key secret 头单请求传入,完整签名/加密
//! 解密由官方实现,返回 task_id(官方已处理 encrypted 形态)。token 与
//! 私钥均不落盘 sidecar。

use codex_agent_identity::AgentIdentityKey;
use codex_agent_identity::register_agent_task;
use http::HeaderMap;

use crate::contract::RawUpstreamResponse;
use crate::contract::RequestContext;
use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::transport::client_pool::ClientPool;
use crate::transport::official_adapter;

/// 官方 agent identity auth API base(与 whoami 同 override,test-upstream)。
fn agent_auth_api_base_url() -> String {
    crate::transport::auth::auth_api_base_url()
}

/// runtime ID 字符集:官方 ID 形态 `[A-Za-z0-9_-]{1,128}`(F-019)。
/// 校验在任何 DNS/socket 之前完成;路径分隔符/控制字符/超长直接拒绝。
fn validate_runtime_id(value: &str) -> Result<(), LocalError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if !valid {
        return Err(LocalError::new(
            "invalid_agent_runtime_id",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "X-CTD-Agent-Runtime-ID must match [A-Za-z0-9_-]{1,128}",
        ));
    }
    Ok(())
}

/// 校验 agent 注册必需参数(runtime id + PKCS8 base64 私钥;空值拒绝)。
fn validate_agent_credentials(ctx: &RequestContext) -> Result<(&str, &str), LocalError> {
    let runtime_id = ctx
        .agent_runtime_id
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            LocalError::new(
                "missing_agent_runtime_id",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "X-CTD-Agent-Runtime-ID is required for agent_task_register",
            )
        })?;
    validate_runtime_id(runtime_id)?;
    let private_key = ctx
        .agent_private_key
        .as_ref()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            LocalError::new(
                "missing_agent_private_key",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "X-CTD-Agent-Private-Key is required for agent_task_register",
            )
        })?;
    Ok((runtime_id, private_key))
}

/// POST /v1/agent/{runtime_id}/task/register(官方组件完整执行)。
pub async fn execute_agent_task_register_raw(
    pool: &ClientPool,
    ctx: &RequestContext,
) -> Result<RawUpstreamResponse, LocalError> {
    let (runtime_id, private_key) = validate_agent_credentials(ctx)?;

    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        Operation::AgentTaskRegister,
    )
    .await?;

    let key = AgentIdentityKey {
        agent_runtime_id: runtime_id,
        private_key_pkcs8_base64: private_key,
    };
    let task_id = register_agent_task(
        &codex_http_client::HttpClient::new(client),
        &agent_auth_api_base_url(),
        key,
    )
    .await
    .map_err(|err| {
        LocalError::new(
            "agent_task_register_failed",
            ErrorClass::Transport,
            ErrorPhase::Connect,
            true,
            format!("agent task registration failed: {err}"),
        )
    })?;

    // 官方已返回 task_id(含 encrypted 解密);透传 200 + task_id JSON,
    // Go 保持 task_id 持久化与 ws 失效语义。
    let body = serde_json::json!({ "task_id": task_id })
        .to_string()
        .into_bytes();
    Ok(RawUpstreamResponse {
        status: http::StatusCode::OK,
        headers: HeaderMap::new(),
        body: Box::pin(futures::stream::once(async move {
            Ok::<_, std::io::Error>(bytes::Bytes::from(body))
        })),
        attempt_count: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::RequestContext;
    use crate::contract::operation::Operation;

    #[test]
    fn agent_credentials_required() {
        let ctx = RequestContext::default();
        assert_eq!(
            validate_agent_credentials(&ctx).unwrap_err().code,
            "missing_agent_runtime_id"
        );
    }

    #[test]
    fn runtime_id_path_safety() {
        for bad in [
            "rt/../evil",
            "rt?query",
            "rt#frag",
            "rt\\back",
            "rt with space",
            "rt%2Fencoded",
            &"x".repeat(129),
        ] {
            let err = validate_runtime_id(bad).expect_err(bad);
            assert_eq!(err.code, "invalid_agent_runtime_id", "{}", bad);
        }
        for good in ["rt-1", "agent_42", "ABCDEFGHIJKLMNOPQRSTUVWXYZ_0123456789"] {
            validate_runtime_id(good).expect(good);
        }
    }

    #[test]
    fn private_key_required_when_runtime_present() {
        let ctx = RequestContext {
            agent_runtime_id: Some("rt-1".to_string()),
            ..RequestContext::default()
        };
        assert_eq!(
            validate_agent_credentials(&ctx).unwrap_err().code,
            "missing_agent_private_key"
        );
    }

    #[test]
    fn empty_values_rejected() {
        // 空私钥:runtime id 存在时先报私钥缺失。
        let ctx = RequestContext {
            agent_runtime_id: Some("rt-1".to_string()),
            agent_private_key: Some(crate::redaction::SecretString::from("")),
            ..RequestContext::default()
        };
        assert_eq!(
            validate_agent_credentials(&ctx).unwrap_err().code,
            "missing_agent_private_key"
        );
        // 空 runtime id:直接报 runtime 缺失(不会触达私钥检查)。
        let ctx2 = RequestContext::default();
        assert_eq!(
            validate_agent_credentials(&ctx2).unwrap_err().code,
            "missing_agent_runtime_id"
        );
    }

    #[test]
    fn valid_credentials_accepted() {
        let ctx = RequestContext {
            agent_runtime_id: Some("rt-1".to_string()),
            agent_private_key: Some(
                "MC4CAQAwBQYDK2VwBCIEIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            ),
            ..RequestContext::default()
        };
        let (runtime_id, key) = validate_agent_credentials(&ctx).expect("valid");
        assert_eq!(runtime_id, "rt-1");
        assert!(!key.is_empty());
    }

    #[test]
    fn operation_route_is_auth_class() {
        assert_eq!(
            Operation::AgentTaskRegister.route_class(),
            codex_http_client::ClientRouteClass::Auth
        );
    }
}
