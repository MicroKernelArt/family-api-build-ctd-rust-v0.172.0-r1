//! 封闭 operation 集（TLS-CTD-001 §11.3 / §16.4）。
//!
//! operation 与 upstream method/path/host 必须是代码中的静态映射；
//! 任何请求都不得携带任意 URL/path/method。

use std::fmt;

use codex_http_client::ClientRouteClass;
use http::Method;

/// 单一事实源（R3-codex，F-0002）：同一 token 流同时生成
/// - `Operation` enum（`Responses` 为 `#[default]`），以及
/// - `Operation::ALL`（全量封闭描述符表，快照导出的唯一迭代入口）。
///
/// 新增 operation 只允许编辑本宏调用：编译期穷尽 match 迫使所有映射臂
/// 同步更新，`ALL` 自动包含新变体，golden 快照随之漂移（导出测试失败）。
macro_rules! define_operations {
    ($( $variant:ident ),* $(,)?) => {
        /// 内部协议封闭 operation。
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub enum Operation {
            #[default]
            Responses,
            $( $variant, )*
        }

        impl Operation {
            /// 全量封闭表（与 enum 同源生成；快照导出只迭代此表）。
            pub const ALL: &'static [Operation] = &[
                Operation::Responses,
                $( Operation::$variant, )*
            ];
        }
    };
}

define_operations!(
    ResponsesCompact,
    ResponsesWebSocketV2,
    Models,
    AlphaSearch,
    Usage,
    QuotaCredits,
    QuotaConsume,
    // P1 平面（T-012/014/015）：独立 contract/wire/canary release slice；
    // 完成对应 gate 前不得出现在 /info.capabilities（INV-015）。
    OAuthRefresh,
    OAuthWhoami,
    FileUpload,
    FileDownload,
    AttachmentDownload,
    FileContentFetch,
    AgentTaskRegister,
    RealtimeCall,
    RealtimeSidebandWS,
);

impl Operation {
    /// 从内部 path（`/_internal/v1/...`）解析 operation；未知路径返回 None。
    pub fn from_internal_path(method: &Method, path: &str) -> Option<Self> {
        let op = match (method.as_str(), path) {
            ("POST", "/_internal/v1/responses") => Self::Responses,
            ("POST", "/_internal/v1/responses/compact") => Self::ResponsesCompact,
            ("GET", "/_internal/v1/responses/ws") => Self::ResponsesWebSocketV2,
            ("GET", "/_internal/v1/models") => Self::Models,
            ("POST", "/_internal/v1/alpha/search") => Self::AlphaSearch,
            ("GET", "/_internal/v1/usage") => Self::Usage,
            ("GET", "/_internal/v1/quota/credits") => Self::QuotaCredits,
            ("POST", "/_internal/v1/quota/consume") => Self::QuotaConsume,
            // P1 平面（独立 release slice）
            ("POST", "/_internal/v1/auth/oauth_refresh") => Self::OAuthRefresh,
            ("GET", "/_internal/v1/auth/whoami") => Self::OAuthWhoami,
            ("POST", "/_internal/v1/files/upload") => Self::FileUpload,
            ("POST", "/_internal/v1/files/download") => Self::FileDownload,
            ("POST", "/_internal/v1/files/attachment") => Self::AttachmentDownload,
            ("POST", "/_internal/v1/files/content") => Self::FileContentFetch,
            ("POST", "/_internal/v1/agent/task_register") => Self::AgentTaskRegister,
            ("POST", "/_internal/v1/realtime/calls") => Self::RealtimeCall,
            ("GET", "/_internal/v1/realtime/sideband") => Self::RealtimeSidebandWS,
            _ => return None,
        };
        Some(op)
    }

    /// 固定 upstream 路径（相对 Provider base_url）。
    pub fn upstream_path(&self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ResponsesCompact => "responses/compact",
            Self::ResponsesWebSocketV2 => "responses",
            Self::Models => "models",
            Self::AlphaSearch => "alpha/search",
            Self::Usage => "wham/usage",
            Self::QuotaCredits => "wham/rate-limit-reset-credits",
            Self::QuotaConsume => "wham/rate-limit-reset-credits/consume",
            Self::OAuthRefresh => "token",
            Self::OAuthWhoami => "api/accounts/v1/user-auth-credential/whoami",
            Self::FileUpload => "files",
            Self::FileDownload => "files/{file_id}/download",
            Self::AttachmentDownload => {
                "conversation/{conversation_id}/attachment/{attachment_id}/download"
            }
            Self::FileContentFetch => "",
            Self::AgentTaskRegister => "v1/agent/{agent_runtime_id}/task/register",
            Self::RealtimeCall => "realtime/calls",
            Self::RealtimeSidebandWS => "realtime/calls/sideband",
        }
    }

    /// 固定 upstream method。
    pub fn upstream_method(&self) -> Method {
        match self {
            Self::Responses
            | Self::ResponsesCompact
            | Self::AlphaSearch
            | Self::QuotaConsume
            | Self::OAuthRefresh
            | Self::FileUpload
            | Self::AgentTaskRegister
            | Self::RealtimeCall => Method::POST,
            Self::ResponsesWebSocketV2
            | Self::Models
            | Self::Usage
            | Self::QuotaCredits
            | Self::OAuthWhoami
            | Self::FileDownload
            | Self::AttachmentDownload
            | Self::FileContentFetch
            | Self::RealtimeSidebandWS => Method::GET,
        }
    }

    /// 低基数观测用的 operation 标签。
    pub fn metric_label(&self) -> &'static str {
        match self {
            Self::Responses => "responses",
            Self::ResponsesCompact => "responses_compact",
            Self::ResponsesWebSocketV2 => "responses_ws_v2",
            Self::Models => "models",
            Self::AlphaSearch => "alpha_search",
            Self::Usage => "usage",
            Self::QuotaCredits => "quota_credits",
            Self::QuotaConsume => "quota_consume",
            Self::OAuthRefresh => "oauth_refresh",
            Self::OAuthWhoami => "oauth_whoami",
            Self::FileUpload => "file_upload",
            Self::FileDownload => "file_download",
            Self::AttachmentDownload => "attachment_download",
            Self::FileContentFetch => "file_content_fetch",
            Self::AgentTaskRegister => "agent_task_register",
            Self::RealtimeCall => "realtime_call",
            Self::RealtimeSidebandWS => "realtime_sideband",
        }
    }

    /// 官方 client route class（代理/连接池语义归属）。
    pub fn route_class(&self) -> ClientRouteClass {
        match self {
            Self::Responses
            | Self::ResponsesCompact
            | Self::Models
            | Self::AlphaSearch
            | Self::Usage
            | Self::QuotaCredits
            | Self::QuotaConsume
            | Self::FileUpload
            | Self::FileDownload
            | Self::AttachmentDownload
            | Self::FileContentFetch
            | Self::RealtimeCall => ClientRouteClass::Api,
            Self::ResponsesWebSocketV2 | Self::RealtimeSidebandWS => ClientRouteClass::WebSocket,
            Self::OAuthRefresh | Self::OAuthWhoami | Self::AgentTaskRegister => {
                ClientRouteClass::Auth
            }
        }
    }

    /// 该 operation 是否要求 session snapshot。
    pub fn requires_session(&self) -> bool {
        matches!(
            self,
            Self::Responses | Self::ResponsesCompact | Self::ResponsesWebSocketV2
        )
    }

    /// 该 operation 的 internal 端点名称（admission 日志/metrics）。
    pub fn internal_endpoint(&self) -> &'static str {
        match self {
            Self::Responses => "/_internal/v1/responses",
            Self::ResponsesCompact => "/_internal/v1/responses/compact",
            Self::ResponsesWebSocketV2 => "/_internal/v1/responses/ws",
            Self::Models => "/_internal/v1/models",
            Self::AlphaSearch => "/_internal/v1/alpha/search",
            Self::Usage => "/_internal/v1/usage",
            Self::QuotaCredits => "/_internal/v1/quota/credits",
            Self::QuotaConsume => "/_internal/v1/quota/consume",
            Self::OAuthRefresh => "/_internal/v1/auth/oauth_refresh",
            Self::OAuthWhoami => "/_internal/v1/auth/whoami",
            Self::FileUpload => "/_internal/v1/files/upload",
            Self::FileDownload => "/_internal/v1/files/download",
            Self::AttachmentDownload => "/_internal/v1/files/attachment",
            Self::FileContentFetch => "/_internal/v1/files/content",
            Self::AgentTaskRegister => "/_internal/v1/agent/task_register",
            Self::RealtimeCall => "/_internal/v1/realtime/calls",
            Self::RealtimeSidebandWS => "/_internal/v1/realtime/sideband",
        }
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.metric_label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_mapping_is_static_and_exhaustive() {
        // R3-codex：遍历唯一全量表 ALL（与 enum 同源宏生成），断言每个
        // operation 的 internal method/path 映射可逆且穷尽。internal method
        // 与 upstream method 是不同契约，文件 operation 会由 POST 映射为 GET。
        for &op in Operation::ALL {
            let (method, path) = match op {
                Operation::Responses => (Method::POST, "/_internal/v1/responses"),
                Operation::ResponsesCompact => (Method::POST, "/_internal/v1/responses/compact"),
                Operation::ResponsesWebSocketV2 => (Method::GET, "/_internal/v1/responses/ws"),
                Operation::Models => (Method::GET, "/_internal/v1/models"),
                Operation::AlphaSearch => (Method::POST, "/_internal/v1/alpha/search"),
                Operation::Usage => (Method::GET, "/_internal/v1/usage"),
                Operation::QuotaCredits => (Method::GET, "/_internal/v1/quota/credits"),
                Operation::QuotaConsume => (Method::POST, "/_internal/v1/quota/consume"),
                Operation::OAuthRefresh => (Method::POST, "/_internal/v1/auth/oauth_refresh"),
                Operation::OAuthWhoami => (Method::GET, "/_internal/v1/auth/whoami"),
                Operation::FileUpload => (Method::POST, "/_internal/v1/files/upload"),
                Operation::FileDownload => (Method::POST, "/_internal/v1/files/download"),
                Operation::AttachmentDownload => (Method::POST, "/_internal/v1/files/attachment"),
                Operation::FileContentFetch => (Method::POST, "/_internal/v1/files/content"),
                Operation::AgentTaskRegister => (Method::POST, "/_internal/v1/agent/task_register"),
                Operation::RealtimeCall => (Method::POST, "/_internal/v1/realtime/calls"),
                Operation::RealtimeSidebandWS => (Method::GET, "/_internal/v1/realtime/sideband"),
            };
            assert_eq!(
                Operation::from_internal_path(&method, path),
                Some(op),
                "internal mapping for {op}"
            );
        }
    }

    #[test]
    fn unknown_or_wrong_method_is_rejected() {
        assert_eq!(
            Operation::from_internal_path(&Method::GET, "/_internal/v1/responses"),
            None
        );
        assert_eq!(
            Operation::from_internal_path(&Method::POST, "/_internal/v1/proxy?url=evil"),
            None
        );
        assert_eq!(
            Operation::from_internal_path(&Method::POST, "/_internal/v2/responses"),
            None
        );
    }

    #[test]
    fn upstream_paths_are_fixed_and_allowlisted() {
        // 防逃生口：upstream path 必须来自固定集合（遍历全量表 ALL）。
        for &op in Operation::ALL {
            let path = op.upstream_path();
            assert!(
                !path.contains("..") && !path.starts_with('/') && !path.contains('?'),
                "upstream path must be a fixed relative path: {path}"
            );
        }
    }

    #[test]
    fn file_operations_use_get_upstream() {
        for operation in [
            Operation::FileDownload,
            Operation::AttachmentDownload,
            Operation::FileContentFetch,
        ] {
            assert_eq!(operation.upstream_method(), Method::GET);
        }
    }

    #[test]
    fn oauth_whoami_path_matches_official_endpoint() {
        let auth_origin = codex_login::auth::PROD_AUTHAPI_BASE_URL
            .strip_suffix("/api/accounts")
            .expect("official auth base includes its fixed path");
        assert_eq!(
            format!("{auth_origin}/{}", Operation::OAuthWhoami.upstream_path()),
            codex_login::auth::whoami_endpoint(codex_login::auth::PROD_AUTHAPI_BASE_URL)
        );
    }

    #[test]
    fn session_requirement_matrix() {
        assert!(Operation::Responses.requires_session());
        assert!(Operation::ResponsesCompact.requires_session());
        assert!(Operation::ResponsesWebSocketV2.requires_session());
        assert!(!Operation::Models.requires_session());
        assert!(!Operation::AlphaSearch.requires_session());
        assert!(!Operation::Usage.requires_session());
        assert!(!Operation::OAuthRefresh.requires_session());
        assert!(!Operation::FileUpload.requires_session());
        assert!(!Operation::RealtimeCall.requires_session());
    }
}
