//! 文件下载平面(P1-C files / OP-012/013/020)。
//!
//! 三个下载操作全部经官方 client factory + 固定 path 模板,无自定义
//! URL 构造:
//! - FileDownload:        GET /backend-api/files/{file_id}/download
//! - AttachmentDownload:  GET /backend-api/conversation/{cid}/attachment/{aid}/download
//! - FileContentFetch:    GET <download_url>（上游动态 URL → url_policy 校验）
//!
//! download_url 仅来自上游响应(OP-020),且必须通过 host allowlist +
//! path 模板校验后才发出请求(INV-006 SSRF 防护)。响应流式透传,
//! token 不落盘,不引入 token provider。

use crate::contract::RawUpstreamResponse;
use crate::contract::RequestContext;
use futures::TryStreamExt as _;

use crate::contract::error::ErrorClass;
use crate::contract::error::ErrorPhase;
use crate::contract::error::LocalError;
use crate::contract::operation::Operation;
use crate::transport::client_pool::ClientPool;
use crate::transport::official_adapter;
use crate::transport::url_policy;

const ID_CHARSET: fn(u8) -> bool = |b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_');

fn require_id<'a>(value: &'a Option<String>, what: &str) -> Result<&'a str, LocalError> {
    let v = value.as_deref().ok_or_else(|| {
        LocalError::new(
            "invalid_file_id",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            format!("missing {what}"),
        )
    })?;
    if v.is_empty() || !v.bytes().all(ID_CHARSET) {
        return Err(LocalError::new(
            "invalid_file_id",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            format!("{what} contains forbidden characters"),
        ));
    }
    Ok(v)
}

async fn raw_get(
    pool: &ClientPool,
    ctx: &RequestContext,
    url: String,
    operation: Operation,
) -> Result<RawUpstreamResponse, LocalError> {
    let handle = official_adapter::resolve_route_handle(ctx.proxy_kind, ctx.proxy_url.as_ref())?;
    let client = official_adapter::pooled_official_client(
        pool,
        &ctx.account_key,
        &ctx.credential_revision,
        &handle,
        operation,
    )
    .await?;
    let response = client
        .get(&url)
        .bearer_auth(ctx.access_token.as_str())
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|err| {
            LocalError::new(
                "file_download_failed",
                ErrorClass::Transport,
                ErrorPhase::Connect,
                true,
                format!("file download request failed: {err}"),
            )
        })?;
    raw_streaming(response, operation).await
}

/// F-011：文件响应必须流式（bytes_stream），禁止 bytes().await 全量缓冲；
/// 客户端取消/断连时 stream drop → 上游连接及时关闭（配合 GuardedBody）。
async fn raw_streaming(
    response: reqwest::Response,
    _operation: Operation,
) -> Result<RawUpstreamResponse, LocalError> {
    let status = response.status();
    let headers = crate::contract::filter_upstream_headers(response.headers());
    let body = response
        .bytes_stream()
        .map_err(|err| std::io::Error::other(err.to_string()));
    Ok(RawUpstreamResponse {
        status,
        headers,
        body: Box::pin(body),
        attempt_count: 1,
    })
}

/// 官方 codex base 的 origin(scheme://authority;test-upstream 下指向
/// fake upstream),下载端点 path 在其上拼接。
fn files_api_origin() -> String {
    let base = url::Url::parse(&official_adapter::chatgpt_provider().base_url)
        .expect("official provider base URL is valid");
    format!("{}://{}", base.scheme(), base.authority())
}

/// GET /backend-api/files/{file_id}/download(OP-012)。
pub async fn execute_file_download(
    pool: &ClientPool,
    ctx: &RequestContext,
) -> Result<RawUpstreamResponse, LocalError> {
    let file_id = require_id(&ctx.file_id, "file id")?;
    raw_get(
        pool,
        ctx,
        format!(
            "{}/backend-api/files/{file_id}/download",
            files_api_origin()
        ),
        Operation::FileDownload,
    )
    .await
}

/// GET /backend-api/conversation/{cid}/attachment/{aid}/download(OP-013)。
pub async fn execute_attachment_download(
    pool: &ClientPool,
    ctx: &RequestContext,
) -> Result<RawUpstreamResponse, LocalError> {
    let cid = require_id(&ctx.conversation_id, "conversation id")?;
    let aid = require_id(&ctx.attachment_id, "attachment id")?;
    raw_get(
        pool,
        ctx,
        format!(
            "{}/backend-api/conversation/{cid}/attachment/{aid}/download",
            files_api_origin()
        ),
        Operation::AttachmentDownload,
    )
    .await
}

/// GET <download_url>(OP-020):download_url 来自上游,必须经 URL 策略校验。
pub async fn execute_file_content_fetch(
    pool: &ClientPool,
    ctx: &RequestContext,
    download_url: &str,
) -> Result<RawUpstreamResponse, LocalError> {
    url_policy::validate_download_url(download_url)?;
    raw_get(
        pool,
        ctx,
        download_url.to_string(),
        Operation::FileContentFetch,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_id_validates_charset() {
        assert_eq!(
            require_id(&None, "file id").unwrap_err().code,
            "invalid_file_id"
        );
        assert_eq!(
            require_id(&Some("file_Ab-1".to_string()), "file id").unwrap(),
            "file_Ab-1"
        );
        assert_eq!(
            require_id(&Some("file ../etc".to_string()), "file id")
                .unwrap_err()
                .code,
            "invalid_file_id"
        );
        assert_eq!(
            require_id(&Some("file%2F..".to_string()), "file id")
                .unwrap_err()
                .code,
            "invalid_file_id"
        );
    }

    #[test]
    fn attachment_ids_validated() {
        assert_eq!(
            require_id(&Some("att..x".to_string()), "attachment id")
                .unwrap_err()
                .code,
            "invalid_file_id"
        );
        assert_eq!(
            require_id(&Some("att_9".to_string()), "attachment id").unwrap(),
            "att_9"
        );
    }
}
