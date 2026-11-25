//! 内部请求 header 契约（TLS-CTD-001 §11.4 / §16.2）。
//!
//! 所有 `X-CTD-*` header 在入口解析后即被消费并丢弃；Rust 从空白
//! `HeaderMap` 经官方 builder 构造 upstream 请求，禁止
//! `headers.extend(internal_request.headers())`。
//!
//! 唯一例外是受控白名单通道 [`CARRIED_HEADERS_HEADER`]：Go 侧已按账号身份
//! scope 完成的官方 `x-codex-*` 元数据以 JSON 承载，解析后只保留
//! [`CARRIED_HEADER_NAMES`] 内的键（凭据键 fail closed），注入时值原样写入
//! 上游投影——它不是"整块 header 继承"，而是逐键白名单。

use std::collections::BTreeMap;
use std::time::Instant;

use http::HeaderMap;
use http::HeaderValue;

use super::error::ErrorClass;
use super::error::ErrorPhase;
use super::error::LocalError;
use crate::contract::operation::Operation;
use crate::redaction::SecretString;

pub const PROTOCOL_VERSION: &str = "1";
pub const PROTOCOL_VERSION_HEADER: &str = "x-ctd-protocol-version";
pub const REQUEST_ID_HEADER: &str = "x-ctd-request-id";
pub const ACCOUNT_KEY_HEADER: &str = "x-ctd-account-key";
pub const CREDENTIAL_REVISION_HEADER: &str = "x-ctd-credential-revision";
pub const ACCESS_TOKEN_HEADER: &str = "x-ctd-access-token";
pub const CHATGPT_ACCOUNT_ID_HEADER: &str = "x-ctd-chatgpt-account-id";
pub const PROXY_KIND_HEADER: &str = "x-ctd-proxy-kind";
pub const PROXY_URL_HEADER: &str = "x-ctd-proxy-url";
pub const SESSION_ID_HEADER: &str = "x-ctd-session-id";
pub const THREAD_ID_HEADER: &str = "x-ctd-thread-id";
pub const WINDOW_ID_HEADER: &str = "x-ctd-window-id";
pub const PURPOSE_HEADER: &str = "x-ctd-purpose";
pub const DEADLINE_UNIX_MS_HEADER: &str = "x-ctd-deadline-unix-ms";
/// admission 等待上限（毫秒；M4：Go cfg.AdmissionTimeoutMS 传入）。
pub const ADMISSION_TIMEOUT_MS_HEADER: &str = "x-ctd-admission-timeout-ms";
pub const IF_NONE_MATCH_HEADER: &str = "x-ctd-if-none-match";
/// OAuth refresh token(P1 auth 平面;仅 UDS,敏感,绝不记录)。
pub const REFRESH_TOKEN_HEADER: &str = "x-ctd-refresh-token";
/// Agent identity 私钥(P1;仅 UDS,敏感,绝不记录)。
pub const AGENT_PRIVATE_KEY_HEADER: &str = "x-ctd-agent-private-key";
/// Agent identity runtime id。
pub const AGENT_RUNTIME_ID_HEADER: &str = "x-ctd-agent-runtime-id";
/// OAuth client_id(auth 平面;Sub2API 自有 OAuth app,非官方常量)。
pub const OAUTH_CLIENT_ID_HEADER: &str = "x-ctd-oauth-client-id";
pub const FILE_ID_HEADER: &str = "x-ctd-file-id";
pub const CONVERSATION_ID_HEADER: &str = "x-ctd-conversation-id";
pub const ATTACHMENT_ID_HEADER: &str = "x-ctd-attachment-id";
/// capture 白名单身份（capture-r1；仅 UDS，Go 从认证后的 API Key 注入；
/// 只允许 capture 模块消费，禁止加入任何上游投影）。
pub const USER_ID_HEADER: &str = "x-ctd-user-id";
/// 白名单出站头承载（FR-004 唯一 header 通道；Go 侧
/// `codex_transport_sidecar.go` 写入，单个 header 承载 JSON object<string,string>）。
/// 编码契约（两端逐字一致）：UTF-8 JSON object，键为小写官方 header 名，
/// 值为 Go 侧已按账号身份 scope 完成的最终出站值。
pub const CARRIED_HEADERS_HEADER: &str = "x-ctd-carried-headers";
/// hookpoint 实现开关（Go 侧 mode=rust_sidecar 且 operation ∈ responses 族时
/// 才会传递）。闭集 `v1|v3`；**缺失 = `v1`**（向后兼容：旧 Go 不传 ⇒ 行为与
/// 今天逐字一致）；未知值 → typed contract error `invalid_hookpoint`，在任何
/// upstream I/O 之前 fail closed。解析形态照抄 [`PROXY_KIND_HEADER`]。
pub const HOOKPOINT_HEADER: &str = "x-ctd-hookpoint";

/// 承载通道交付的 installation 身份头名（**消费型**白名单项，见
/// [`CARRIED_HEADER_NAMES`] 的说明）。单一定义点：白名单、projection 取值与
/// v1 出站头集的消费点都引用它，避免第三处字面量漂移。
pub const CARRIED_INSTALLATION_ID_KEY: &str = "x-codex-installation-id";

/// 允许承载到上游投影的官方 header 白名单（小写）。
/// 与 Go 侧 `codex_transport_integration.go` 的 `codexCarriedHeaderNames`
/// **逐字相等**；任何一侧增删都会让上游投影与 R-Go 直连发生漂移。
///
/// 有意不纳入：
/// - `version`：跨语言契约显式禁止携带，且由官方 provider 的 version header
///   （`CARGO_PKG_VERSION`）产出，承载它会让 sidecar 谎报自身构建版本；
/// - `x-codex-parent-thread-id` / `x-openai-subagent`：subagent 作用域，
///   不在 R-Go 对拍范围。
///
/// `x-client-request-id` 纳入：官方不变式是「x-client-request-id == session-id」，
/// Go 侧的 md 投影（applyCodexSessionMetadataProjection）据此把它派生为
/// md.SessionID，R-Go 直连会真的把它发出去；若承载通道不含它，R-CTD 就会少一个
/// R-Go 有的头，两侧对拍立刻出现差异。
///
/// `user-agent` / `originator` 纳入：R-Go 侧是网关归一化值，R-CTD 侧由官方
/// generator 在 sidecar 自身构建环境求值（os_info/arch/terminal、
/// `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`），只有承载通道能对齐两者。
///
/// `x-codex-installation-id` 纳入（**消费型**，2026-10-02）：它的交付语义与
/// 上表其余项**不同**——不是"投影覆盖/原样出站"，而是 projection 的
/// `installation_id` **来源**（见 [`crate::transport::metadata::effective_installation_id`]）。
///
/// 原因：R-CTD 自身的 installation 身份是**进程级单值**
/// （`bootstrap.rs::ensure_installation_id` → `ServerState.installation_id`），
/// 与上游账号无关；而 v1 `/responses` 出站 body 的
/// `client_metadata["x-codex-installation-id"]` 正是由它注入，Go 交付的账号值在
/// 交付前被 reserved 剥离 ⇒ **所有账号共用同一安装标识**（跨账号关联向量）。
/// Go 侧现已按上游账号派生该值（`resolveAccountInstallationID`），承载通道是它
/// 到达 sidecar 的唯一途径。
///
/// ★ 该值**绝不出现在 v1 出站头集**：官方 `/responses` 不发这个头（账号隔离由
/// body `client_metadata` 表达），发出会破坏已对齐的官方头构造序。见
/// [`crate::transport::responses::outbound_headers`] 的消费点（`/responses`）与
/// [`crate::transport::compact::compact_outbound_headers`] 的注入点（compact，
/// 官方在那里确实发这个头）。
///
/// ★ 历史注释曾写「installation 身份由 sidecar 从 account_key 确定性派生」——
/// **代码里不存在该派生**：`account_key` 只用于连接池 key
/// （`transport/client_pool.rs`），实际来源始终是进程级单值。
pub const CARRIED_HEADER_NAMES: &[&str] = &[
    "session-id",
    "thread-id",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-codex-beta-features",
    "x-codex-routing-hint",
    "x-openai-internal-codex-responses-lite",
    "x-client-request-id",
    "openai-beta",
    "user-agent",
    "originator",
    CARRIED_INSTALLATION_ID_KEY,
];

/// 凭据硬护栏：这些名字绝不进入上游投影，出现即 fail closed
/// （与 Go 侧 `codexCarriedHeaderDenylist` 逐字相等）。
pub const CARRIED_HEADER_DENYLIST: &[&str] = &["authorization", "chatgpt-account-id", "cookie"];

/// v3（hookpoint=v3）完整出站头承载。Go 侧
/// `codex_transport_sidecar.go` 写入，单个 header 承载 JSON
/// object<string,string>；编码契约与 [`CARRIED_HEADERS_HEADER`] 逐字一致
/// （UTF-8 JSON object，键为小写官方 header 名），只是白名单表换成
/// [`V3_HEADER_ALLOWLIST`]、语义从"投影覆盖"变为"完整出站头集"。
pub const OUTBOUND_HEADERS_HEADER: &str = "x-ctd-outbound-headers";

/// v3 允许承载的官方出站头白名单（小写）。
///
/// 生成规则（docs/20 §3.2，不是拍脑袋写的）：
/// ```text
/// V3_HEADER_ALLOWLIST
///   = 官方 0.159.2 responses 出站头集合（18 头，docs/21 §3.1 实测）
///     ∩ ¬( ctd-owned 传输头 ∪ 凭据硬护栏 )
/// ```
/// 被剔除的四项：`authorization` / `chatgpt-account-id`（凭据，由 ctd 在
/// `auth.apply_auth` 注入）与 `host` / `content-length`（传输栈按最终字节
/// 生成）。`accept-encoding` 同为传输栈自有，不在官方 18 头内。
///
/// 与 Go 侧 `codexV3HeaderNames` **逐字相等**，由漂移测试钉死
/// （`codex_transport_v3_headers_test.go`）。
///
/// 有意不纳入：`openai-beta`（官方 HTTP POST 不发；R-Go 的 HTTP 传输策略
/// 同样剥离，见 `applyCodexTransportHeaders` 的 `CodexTransportHTTP` 分支）、
/// `x-codex-installation-id`（官方 `/responses` 不作为 header 发，v3 下账号
/// 隔离由 body 原样透传保证）——**后者在 compact 下是例外**，见
/// [`V3_COMPACT_EXTRA_HEADER_NAMES`]。
pub const V3_HEADER_ALLOWLIST: &[&str] = &[
    "version",
    "x-codex-beta-features",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-openai-internal-codex-responses-lite",
    "x-codex-routing-hint",
    "x-client-request-id",
    "session-id",
    "thread-id",
    "accept",
    "content-encoding",
    "content-type",
    "originator",
    "user-agent",
];

/// **compact 专属**的 v3 出站头（operation 收窄的追加白名单，小写）。
///
/// 与 [`V3_HEADER_ALLOWLIST`] 的关系：白名单是**并集**的两半——responses 恒取
/// 前者，compact 取「前者 ∪ 本表」。之所以不合并进 `V3_HEADER_ALLOWLIST`：
///
/// - 该表与 Go 侧 `codexV3HeaderNames` 逐字相等、由漂移测试钉死，且是官方
///   `/responses` 18 头（docs/21 §3.1 实测）的过滤依据——把 compact 专属头
///   并进去，会让 `/responses` **多出**一个官方没有的头（回归）；
/// - 头存在性在官方两侧并不同源：官方 `/responses` 不发
///   `x-codex-installation-id`（账号隔离由 `body.client_metadata` 原样透传），
///   而 compact route 的官方 client 发它。
///
/// 唯一的成员 `x-codex-installation-id`：官方**最后一个**使用该 route 的
/// client（`rust-v0.153.4` 的 `CompactClient`）在 extra-headers 段**首位**插入
/// 它，官方测试亦断言存在（`core/tests/suite/compact_remote.rs`：
/// `compact_request.header("x-codex-installation-id")`）⇒ 它在 v3 compact 下
/// 由 Go 交付、ctd 放行并落在 `version` 之后。
///
/// ★ 位次依据（**不是官方实测序**）：官方 0.159.2 **不存在**
/// `/responses/compact` 请求（pristine `codex-api` 无 compact client，
/// `official-1592/flows-*.jsonl` 无该 route 的请求行，docs/23 §3.2 U2），
/// compact 头序无官方样本 [未覆盖]。本表的位次沿用 **v1 侧的既有判定**
/// （`transport/compact.rs::compact_outbound_headers` 的同一结论：
/// installation 头紧跟 provider 的 `version`、先于元数据段）。
pub const V3_COMPACT_EXTRA_HEADER_NAMES: &[&str] = &["x-codex-installation-id"];

/// v3 必需头：v3 下 ctd 不做任何 header 决策（不补 `content-type`、不写
/// `accept`），缺失即上游 4xx ⇒ 必须在任何 upstream I/O 之前 fail closed
/// （typed error `missing_v3_required_header`，docs/20 §3.2 / §3.3 R6）。
pub const V3_REQUIRED_HEADERS: &[&str] = &["accept", "content-type"];

/// v3 审计所需头（docs/20 §3.5 项 1–4）：v3 下 ctd 从"共同权威"降为
/// "纯消费者 + 审计者"，这些头内自洽不变式是审计对象。
/// 缺失或自洽性破坏 ⇒ typed error `hookpoint_audit_failed`（fail closed）。
pub const V3_AUDIT_HEADERS: &[&str] = &[
    "session-id",
    "thread-id",
    "x-codex-window-id",
    "x-client-request-id",
    "x-codex-turn-metadata",
];

/// v3 `content-encoding` 闭集（缺省 = identity）。见 docs/20 §3.3 R4：
/// 防止 v3 契约被当成"任意透传口"，也防止引入 ctd/上游链路未验证的编码
/// （`gzip` / `br` / `deflate`）。
pub const V3_CONTENT_ENCODINGS: &[&str] = &["zstd"];

/// 请求目的（仅低基数观测，不成为 upstream header/body 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestPurpose {
    #[default]
    Request,
    Probe,
    AccountTest,
}

impl RequestPurpose {
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("probe") => Self::Probe,
            Some("account_test") => Self::AccountTest,
            _ => Self::Request,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Probe => "probe",
            Self::AccountTest => "account_test",
        }
    }
}

/// 解析后的不可变请求上下文（secret 已封装）。
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    pub request_id: String,
    pub account_key: String,
    pub credential_revision: String,
    pub access_token: SecretString,
    pub chatgpt_account_id: Option<SecretString>,
    pub operation: Operation,
    pub proxy_kind: ProxyKind,
    pub proxy_url: Option<SecretString>,
    pub session: Option<SessionSnapshot>,
    pub purpose: RequestPurpose,
    pub deadline: Option<Instant>,
    /// admission 等待上限（毫秒；M4，缺失用常量）。
    pub admission_timeout_ms: Option<u64>,
    pub if_none_match: Option<String>,
    /// OAuth refresh token(仅 auth operation 使用)。
    pub refresh_token: Option<SecretString>,
    /// Agent identity 私钥(PKCS8 base64,仅 agent operation 使用)。
    pub agent_private_key: Option<SecretString>,
    pub agent_runtime_id: Option<String>,
    pub oauth_client_id: Option<String>,
    pub file_id: Option<String>,
    pub conversation_id: Option<String>,
    pub attachment_id: Option<String>,
    /// capture 白名单身份（capture-r1；缺省 None，不 fail；仅 capture 模块消费）。
    pub user_id: Option<String>,
    /// 白名单出站头承载（FR-004）：Go 侧已 scope 完成的官方 x-codex-* 元数据。
    /// 解析时只保留 [`CARRIED_HEADER_NAMES`]，凭据键 fail closed；注入端原样
    /// 写入上游投影（覆盖 sidecar 本地投影）。
    pub carried_headers: BTreeMap<String, String>,
    /// v3 完整出站头承载（[`OUTBOUND_HEADERS_HEADER`]）：**仅当
    /// `hookpoint == v3` 时解析**，其余情况恒为空 map。
    ///
    /// ★ 为什么按 hookpoint 条件解析：v1 路径必须与"没有 v3 实现之前"
    /// 逐字相同——v1 请求即使（错误地）携带该头也不得改变解析结果。
    /// 解析时只保留 [`V3_HEADER_ALLOWLIST`]，凭据键 fail closed；
    /// 注入端原样写入（v3 下 ctd 不做任何 header 决策）。
    pub outbound_headers: BTreeMap<String, String>,
    /// hookpoint 开关（v1/v3；缺失 = v1）。v1 = ctd 自建请求（默认实现，
    /// 与开关落地前逐字相同）；v3 = ctd 原样转发 Go 交付的完整出站头集
    /// 与最终 body 字节。
    pub hookpoint: Hookpoint,
    /// 内部 traceparent（只建立 Go→Rust child span；不得复制到 upstream）。
    pub traceparent: Option<String>,
}

/// 账号级 proxy route（TLS-CTD-001 §14）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyKind {
    #[default]
    Direct,
    Http,
    Https,
    Socks5,
    Socks5h,
}

impl ProxyKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "direct" => Some(Self::Direct),
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks5" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }
}

/// Codex hookpoint 实现选择（v1/v3）。闭集枚举，解析形态与 [`ProxyKind`]
/// 逐字同形。
///
/// 作用域：hookpoint 只对 `responses` / `responses_compact` 有定义；Go 侧
/// 对其它 operation 一律按 v1 处理且不传该头。若越界传入 v3，Rust 侧以
/// typed contract error `invalid_hookpoint_scope` fail closed。
///
/// 本任务只做开关接线：`v3` 与 `v1` 走**同一条实现路径**（携带 v3 标记），
/// 不引入任何 body/header 行为差异。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Hookpoint {
    #[default]
    V1,
    V3,
}

impl Hookpoint {
    /// 闭集解析：未知值返回 `None`（调用方 fail closed）。
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "v1" => Some(Self::V1),
            "v3" => Some(Self::V3),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V3 => "v3",
        }
    }

    /// 该 hookpoint 是否对给定 operation 有定义（作用域收窄）。
    pub fn supports_operation(&self, operation: Operation) -> bool {
        match self {
            // v1 是默认实现，任何 operation 都按 v1 执行。
            Self::V1 => true,
            // v3 只覆盖 responses 族（§2.3）。
            Self::V3 => matches!(
                operation,
                Operation::Responses | Operation::ResponsesCompact
            ),
        }
    }
}

/// 经验证的 v0_144 session 不可变快照（TLS-CTD-001 §11.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub thread_id: String,
    pub window_id: String,
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value: &HeaderValue| value.to_str().ok())
}

fn required(headers: &HeaderMap, name: &str, err: &mut Option<LocalError>) -> Option<String> {
    match header_str(headers, name) {
        Some(value) if !value.is_empty() => Some(value.to_string()),
        _ => {
            if err.is_none() {
                *err = Some(super::error::codes::missing_required_header(name));
            }
            None
        }
    }
}

/// 校验协议版本与必需 header，并构造不可变请求上下文。
///
/// 该函数在 admission 阶段（任何 DNS/proxy/upstream I/O 之前）完成全部
/// 字段校验（TLS-CTD-001 §16.2 顺序 2–6）。
pub fn parse_request_context(
    headers: &HeaderMap,
    operation: Operation,
) -> Result<RequestContext, LocalError> {
    // 2. protocol version
    let version = header_str(headers, PROTOCOL_VERSION_HEADER).unwrap_or("");
    if version != PROTOCOL_VERSION {
        return Err(super::error::codes::invalid_protocol_version());
    }

    let mut err: Option<LocalError> = None;
    let request_id = required(headers, REQUEST_ID_HEADER, &mut err);
    let account_key = required(headers, ACCOUNT_KEY_HEADER, &mut err);
    let credential_revision = required(headers, CREDENTIAL_REVISION_HEADER, &mut err);
    let access_token = required(headers, ACCESS_TOKEN_HEADER, &mut err);
    if let Some(e) = err {
        return Err(e);
    }

    // 4. 字符集与字段形状
    let request_id = request_id.expect("checked above");
    let account_key = account_key.expect("checked above");
    let credential_revision = credential_revision.expect("checked above");
    let access_token = access_token.expect("checked above");
    if request_id.len() > 128
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == b'-')
    {
        return Err(LocalError::new(
            "invalid_request_id",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "X-CTD-Request-ID failed character/format validation",
        ));
    }
    if account_key.len() > 128 {
        return Err(LocalError::new(
            "invalid_account_key",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "X-CTD-Account-Key exceeds the length limit",
        ));
    }

    // 5. proxy kind/url
    let proxy_kind_raw = header_str(headers, PROXY_KIND_HEADER).unwrap_or("direct");
    let proxy_kind = ProxyKind::parse(proxy_kind_raw).ok_or_else(|| {
        LocalError::new(
            "invalid_proxy_kind",
            ErrorClass::Contract,
            ErrorPhase::ResolveProxy,
            false,
            "unknown X-CTD-Proxy-Kind",
        )
    })?;
    let proxy_url = match proxy_kind {
        ProxyKind::Direct => None,
        _ => {
            let raw = header_str(headers, PROXY_URL_HEADER).ok_or_else(|| {
                LocalError::new(
                    "missing_proxy_url",
                    ErrorClass::Contract,
                    ErrorPhase::ResolveProxy,
                    false,
                    "X-CTD-Proxy-URL is required for non-direct routes",
                )
            })?;
            if raw.len() > 2048 {
                return Err(LocalError::new(
                    "invalid_proxy_url",
                    ErrorClass::Contract,
                    ErrorPhase::ResolveProxy,
                    false,
                    "X-CTD-Proxy-URL exceeds the length limit",
                ));
            }
            Some(SecretString::from(raw.to_string()))
        }
    };

    // 5b. hookpoint（闭集 v1|v3；缺失 = v1；未知值 / 越界 fail closed）。
    // 与 proxy kind 同段、同形；同样在任何 upstream I/O 之前完成。
    let hookpoint = match header_str(headers, HOOKPOINT_HEADER) {
        None => Hookpoint::V1,
        Some(raw) => Hookpoint::parse(raw).ok_or_else(|| {
            LocalError::new(
                "invalid_hookpoint",
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                "unknown X-CTD-Hookpoint",
            )
        })?,
    };
    if !hookpoint.supports_operation(operation) {
        return Err(LocalError::new(
            "invalid_hookpoint_scope",
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            "X-CTD-Hookpoint=v3 is only defined for responses operations",
        ));
    }

    // 6. session snapshot
    let session = if operation.requires_session() {
        let session_id = required(headers, SESSION_ID_HEADER, &mut err);
        let thread_id = required(headers, THREAD_ID_HEADER, &mut err);
        let window_id = required(headers, WINDOW_ID_HEADER, &mut err);
        if let Some(e) = err {
            return Err(e);
        }
        let snapshot = SessionSnapshot {
            session_id: session_id.expect("checked above"),
            thread_id: thread_id.expect("checked above"),
            window_id: window_id.expect("checked above"),
        };
        validate_session_snapshot(&snapshot)?;
        Some(snapshot)
    } else {
        None
    };

    // F-015：deadline 语义——缺失=operation 默认；格式错/非正=contract
    // error（不静默 300s）；已过=立即过期（server 侧 deadline_exceeded）。
    // R4-M1：admission header 存在时必须解析为显式正整数且不超过上限；
    // 非法值 fail closed（不静默降级到默认）。
    let admission_timeout_ms = match header_str(headers, ADMISSION_TIMEOUT_MS_HEADER) {
        None => None,
        Some(raw) => {
            let millis = raw.trim().parse::<u64>().map_err(|_| {
                LocalError::new(
                    "invalid_admission_timeout",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    format!("X-CTD-Admission-Timeout-Ms 非法值 {raw:?}"),
                )
            })?;
            if millis == 0 || millis > 60_000 {
                return Err(LocalError::new(
                    "invalid_admission_timeout",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    "X-CTD-Admission-Timeout-Ms 必须在 (0, 60000] ms",
                ));
            }
            Some(millis)
        }
    };
    let deadline: Option<Instant> = match header_str(headers, DEADLINE_UNIX_MS_HEADER) {
        None => None,
        Some(raw) => {
            let ms = raw.trim().parse::<i64>().map_err(|_| {
                LocalError::new(
                    "invalid_deadline",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    format!("X-CTD-Deadline-Unix-Ms 非法值 {raw:?}"),
                )
            })?;
            if ms <= 0 {
                return Err(LocalError::new(
                    "invalid_deadline",
                    ErrorClass::Contract,
                    ErrorPhase::Validate,
                    false,
                    "X-CTD-Deadline-Unix-Ms 必须为正毫秒时间戳",
                ));
            }
            let remaining_ms = ms - now_ms() as i64;
            Some(if remaining_ms <= 0 {
                Instant::now() // 已过：立即过期
            } else {
                Instant::now() + std::time::Duration::from_millis(remaining_ms as u64)
            })
        }
    };

    Ok(RequestContext {
        request_id,
        account_key,
        credential_revision,
        access_token: SecretString::from(access_token),
        chatgpt_account_id: header_str(headers, CHATGPT_ACCOUNT_ID_HEADER)
            .filter(|v| !v.is_empty())
            .map(|v| SecretString::from(v.to_string())),
        operation,
        proxy_kind,
        proxy_url,
        session,
        purpose: RequestPurpose::parse(header_str(headers, PURPOSE_HEADER)),
        admission_timeout_ms,
        deadline,
        if_none_match: header_str(headers, IF_NONE_MATCH_HEADER)
            .filter(|v| !v.is_empty())
            .map(ToString::to_string),
        refresh_token: header_str(headers, REFRESH_TOKEN_HEADER)
            .filter(|v| !v.is_empty())
            .map(|v| SecretString::from(v.to_string())),
        agent_private_key: header_str(headers, AGENT_PRIVATE_KEY_HEADER)
            .filter(|v| !v.is_empty())
            .map(|v| SecretString::from(v.to_string())),
        agent_runtime_id: header_str(headers, AGENT_RUNTIME_ID_HEADER)
            .filter(|v| !v.is_empty())
            .map(ToString::to_string),
        oauth_client_id: header_str(headers, OAUTH_CLIENT_ID_HEADER).map(String::from),
        file_id: header_str(headers, FILE_ID_HEADER).map(String::from),
        conversation_id: header_str(headers, CONVERSATION_ID_HEADER).map(String::from),
        attachment_id: header_str(headers, ATTACHMENT_ID_HEADER)
            .filter(|v| !v.is_empty())
            .map(String::from),
        user_id: header_str(headers, USER_ID_HEADER).map(String::from),
        carried_headers: parse_carried_headers(
            header_str(headers, CARRIED_HEADERS_HEADER),
            operation,
        )?,
        // ★ v3 通道只在 hookpoint==v3 时解析：v1 请求（含错误携带该头者）
        // 的解析结果必须与"没有 v3 实现之前"逐字相同。
        // ★ 白名单按 operation 收窄：compact 专属头（[`V3_COMPACT_EXTRA_HEADER_NAMES`]）
        // 只在 `responses_compact` 下放行，`/responses` 下仍被静默丢弃。
        outbound_headers: if hookpoint == Hookpoint::V3 {
            parse_outbound_headers(header_str(headers, OUTBOUND_HEADERS_HEADER), operation)?
        } else {
            BTreeMap::new()
        },
        hookpoint,
        traceparent: header_str(headers, "traceparent")
            .filter(|v| !v.is_empty())
            .map(ToString::to_string),
    })
}

/// 承载通道规格：头名 + 白名单 + 错误码。v1（`X-CTD-Carried-Headers`）与
/// v3（`X-CTD-Outbound-Headers`）共用同一解析器与同一凭据硬护栏，只有
/// 白名单表与错误码不同（"复用既有模式，不新造第二套"）。
struct HeaderCarrierSpec {
    /// 错误正文里的展示名（诊断用，与 header 名逐字一致的大写形态）。
    label: &'static str,
    allowlist: &'static [&'static str],
    /// operation 收窄的**追加**白名单：只有列出的 operation 才放行这些头名。
    /// 空表 = 无例外（v1 承载通道恒为空）。
    ///
    /// 存在的原因见 [`V3_COMPACT_EXTRA_HEADER_NAMES`]：v3 的头存在性按
    /// operation 分叉（`x-codex-installation-id` 只在 compact 出现），
    /// 用"单表白名单 + operation 例外"表达，避免第二张 15 项的白名单表。
    operation_extras: &'static [(Operation, &'static [&'static str])],
    invalid_code: &'static str,
    credential_code: &'static str,
}

/// v1 承载通道（FR-004 唯一 header 通道）。
const CARRIED_CARRIER: HeaderCarrierSpec = HeaderCarrierSpec {
    label: "X-CTD-Carried-Headers",
    allowlist: CARRIED_HEADER_NAMES,
    operation_extras: &[],
    invalid_code: "invalid_carried_headers",
    credential_code: "carried_headers_credential_rejected",
};

/// v3 承载通道（hookpoint=v3 的完整出站头集）。
const OUTBOUND_CARRIER: HeaderCarrierSpec = HeaderCarrierSpec {
    label: "X-CTD-Outbound-Headers",
    allowlist: V3_HEADER_ALLOWLIST,
    operation_extras: &[(Operation::ResponsesCompact, V3_COMPACT_EXTRA_HEADER_NAMES)],
    invalid_code: "invalid_outbound_headers",
    credential_code: "outbound_headers_credential_rejected",
};

/// 解析并校验白名单承载头（[`CARRIED_HEADERS_HEADER`]）。
///
/// 编码契约与 Go 侧 `codex_transport_sidecar.go::encodeCodexCarriedHeaders`
/// 逐字一致：UTF-8 JSON object<string,string>，键为小写官方 header 名。
///
/// 解析规则（fail closed）：
/// - 缺失/空白 → 空 map（不承载，等价于旧行为）；
/// - 非法 JSON / 非 object<string,string> / 非法 header 名或值 → typed
///   contract error（在任何 upstream I/O 之前）；
/// - 凭据键（[`CARRIED_HEADER_DENYLIST`]）→ typed contract error；
/// - 白名单外的键 → 丢弃（结构上不可能进入上游投影）。
fn parse_carried_headers(
    raw: Option<&str>,
    operation: Operation,
) -> Result<BTreeMap<String, String>, LocalError> {
    parse_header_carrier(&CARRIED_CARRIER, raw, operation)
}

/// 解析并校验 v3 完整出站头承载（[`OUTBOUND_HEADERS_HEADER`]）。
/// 与 [`parse_carried_headers`] 同一实现，只换白名单表（+ compact 专属例外）
/// 与错误码。
fn parse_outbound_headers(
    raw: Option<&str>,
    operation: Operation,
) -> Result<BTreeMap<String, String>, LocalError> {
    parse_header_carrier(&OUTBOUND_CARRIER, raw, operation)
}

fn parse_header_carrier(
    spec: &HeaderCarrierSpec,
    raw: Option<&str>,
    operation: Operation,
) -> Result<BTreeMap<String, String>, LocalError> {
    let Some(raw) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(BTreeMap::new());
    };
    let decoded: BTreeMap<String, String> = serde_json::from_str(raw).map_err(|_| {
        LocalError::new(
            spec.invalid_code,
            ErrorClass::Contract,
            ErrorPhase::Validate,
            false,
            format!("{} 不是合法的 JSON object<string,string>", spec.label),
        )
    })?;
    let mut carried = BTreeMap::new();
    for (name, value) in decoded {
        let name = name.trim().to_ascii_lowercase();
        if CARRIED_HEADER_DENYLIST.contains(&name.as_str()) {
            // 只报 header 名，绝不回显值（值可能是凭据）。
            return Err(LocalError::new(
                spec.credential_code,
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                format!("承载头禁止携带凭据头 {name}"),
            ));
        }
        // ★ operation 收窄：白名单命中，或该头是**本 operation** 的例外
        // （`/responses` 下 compact 专属头仍然被静默丢弃）。
        let allowed = spec.allowlist.contains(&name.as_str())
            || spec
                .operation_extras
                .iter()
                .any(|(scoped, names)| *scoped == operation && names.contains(&name.as_str()));
        if !allowed || value.is_empty() {
            continue;
        }
        if http::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || HeaderValue::from_str(&value).is_err()
        {
            return Err(LocalError::new(
                spec.invalid_code,
                ErrorClass::Contract,
                ErrorPhase::Validate,
                false,
                format!("承载头 {name} 的名字或值不是合法 HTTP header"),
            ));
        }
        carried.insert(name, value);
    }
    Ok(carried)
}

impl RequestContext {
    /// 承载头 → 上游投影 `HeaderMap`（仅白名单，值原样注入）。
    ///
    /// 解析期已完成名字/值合法性校验，因此这里只做逐键搬运，不做任何
    /// 归一化/重编码（保持与 R-Go 直连逐字节一致）。
    pub fn carried_header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.carried_headers {
            let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            let Ok(value) = HeaderValue::from_str(value) else {
                continue;
            };
            headers.insert(name, value);
        }
        headers
    }

    /// v3 完整出站头 → 上游投影 `HeaderMap`（仅 [`V3_HEADER_ALLOWLIST`] ∪
    /// 本 operation 的 [`V3_COMPACT_EXTRA_HEADER_NAMES`]，
    /// 值原样注入，**不补任何头**）。
    ///
    /// ★ v3 下 ctd 不做 header 决策：`content-type` / `accept` 由 Go 交付，
    /// ctd 只做白名单过滤（`host` / `content-length` / `accept-encoding` 与
    /// 凭据头在结构上不可能进入——前者不在白名单，后者解析期 fail closed）。
    /// 缺失即 [`V3_REQUIRED_HEADERS`] 校验 fail closed，ctd 绝不代补。
    pub fn outbound_header_map(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in &self.outbound_headers {
            let Ok(name) = http::header::HeaderName::from_bytes(name.as_bytes()) else {
                continue;
            };
            let Ok(value) = HeaderValue::from_str(value) else {
                continue;
            };
            headers.insert(name, value);
        }
        headers
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 校验 v0_144 session/thread/window（UUIDv7 + window 格式）。
pub fn validate_session_snapshot(snapshot: &SessionSnapshot) -> Result<(), LocalError> {
    codex_core::CodexResponsesMetadata::try_for_transport(
        "00000000-0000-0000-0000-000000000000".to_string(),
        snapshot.session_id.clone(),
        snapshot.thread_id.clone(),
        snapshot.window_id.clone(),
    )
    .map_err(|_| super::error::codes::invalid_session())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::operation::Operation;

    fn base_headers() -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(PROTOCOL_VERSION_HEADER, HeaderValue::from_static("1"));
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_123"));
        headers.insert(ACCOUNT_KEY_HEADER, HeaderValue::from_static("ak-hmac"));
        headers.insert(
            CREDENTIAL_REVISION_HEADER,
            HeaderValue::from_static("rev-1"),
        );
        headers.insert(ACCESS_TOKEN_HEADER, HeaderValue::from_static("sk-secret"));
        headers.insert(PROXY_KIND_HEADER, HeaderValue::from_static("direct"));
        headers.insert(
            SESSION_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000001"),
        );
        headers.insert(
            THREAD_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002"),
        );
        headers.insert(
            WINDOW_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002:1"),
        );
        headers
    }

    #[test]
    fn parses_valid_request_context() {
        let ctx = parse_request_context(&base_headers(), Operation::Responses).expect("parse");
        assert_eq!(ctx.request_id, "req_123");
        assert_eq!(ctx.proxy_kind, ProxyKind::Direct);
        assert!(ctx.proxy_url.is_none());
        assert_eq!(
            ctx.session.as_ref().expect("session").session_id,
            "0198f2b0-0000-7000-8000-000000000001"
        );
    }

    #[test]
    fn rejects_wrong_protocol_version() {
        let mut headers = base_headers();
        headers.insert(PROTOCOL_VERSION_HEADER, HeaderValue::from_static("2"));
        let err = parse_request_context(&headers, Operation::Responses).expect_err("must fail");
        assert_eq!(err.code, "invalid_protocol_version");
    }

    #[test]
    fn rejects_missing_required_header() {
        let mut headers = base_headers();
        headers.remove(ACCESS_TOKEN_HEADER);
        let err = parse_request_context(&headers, Operation::Responses).expect_err("must fail");
        assert_eq!(err.code, "missing_required_header");
    }

    #[test]
    fn rejects_invalid_window_format() {
        let mut headers = base_headers();
        headers.insert(WINDOW_ID_HEADER, HeaderValue::from_static("not-a-window"));
        let err = parse_request_context(&headers, Operation::Responses).expect_err("must fail");
        assert_eq!(err.code, "invalid_session");
    }

    #[test]
    fn rejects_unknown_proxy_kind() {
        let mut headers = base_headers();
        headers.insert(PROXY_KIND_HEADER, HeaderValue::from_static("gopher"));
        let err = parse_request_context(&headers, Operation::Responses).expect_err("must fail");
        assert_eq!(err.code, "invalid_proxy_kind");
    }

    #[test]
    fn absent_hookpoint_defaults_to_v1() {
        // 向后兼容：旧 Go 不传头 ⇒ v1（行为与今天逐字一致）。
        let ctx = parse_request_context(&base_headers(), Operation::Responses).expect("parse");
        assert_eq!(ctx.hookpoint, Hookpoint::V1);
    }

    #[test]
    fn explicit_v1_is_equivalent_to_absent() {
        // ★ 默认配置（Go 发 v1）与旧 Go（不传头）必须解析出完全相同的上下文：
        // 除 hookpoint 字段本身外逐字段相等 ⇒ v1 与"今天"走同一条实现路径。
        let absent = parse_request_context(&base_headers(), Operation::Responses).expect("parse");
        let mut headers = base_headers();
        headers.insert(HOOKPOINT_HEADER, HeaderValue::from_static("v1"));
        let explicit = parse_request_context(&headers, Operation::Responses).expect("parse");

        assert_eq!(absent.hookpoint, Hookpoint::V1);
        assert_eq!(explicit.hookpoint, Hookpoint::V1);
        assert_eq!(explicit.operation, absent.operation);
        assert_eq!(explicit.request_id, absent.request_id);
        assert_eq!(explicit.account_key, absent.account_key);
        assert_eq!(explicit.credential_revision, absent.credential_revision);
        assert_eq!(explicit.proxy_kind, absent.proxy_kind);
        assert_eq!(explicit.purpose, absent.purpose);
        assert_eq!(explicit.session, absent.session);
        assert_eq!(explicit.carried_headers, absent.carried_headers);
        assert_eq!(explicit.deadline.is_some(), absent.deadline.is_some());
        assert_eq!(explicit.admission_timeout_ms, absent.admission_timeout_ms);
    }

    #[test]
    fn hookpoint_closed_set_parses() {
        for (raw, want) in [("v1", Hookpoint::V1), ("v3", Hookpoint::V3)] {
            let mut headers = base_headers();
            headers.insert(HOOKPOINT_HEADER, HeaderValue::from_str(raw).expect("value"));
            let ctx = parse_request_context(&headers, Operation::Responses).expect("parse");
            assert_eq!(ctx.hookpoint, want);
            assert_eq!(ctx.hookpoint.as_str(), raw);
        }
        // compact 同属 responses 族。
        let mut headers = base_headers();
        headers.insert(HOOKPOINT_HEADER, HeaderValue::from_static("v3"));
        let ctx = parse_request_context(&headers, Operation::ResponsesCompact).expect("parse");
        assert_eq!(ctx.hookpoint, Hookpoint::V3);
    }

    #[test]
    fn rejects_unknown_hookpoint() {
        let mut headers = base_headers();
        headers.insert(HOOKPOINT_HEADER, HeaderValue::from_static("v2"));
        let err = parse_request_context(&headers, Operation::Responses).expect_err("must fail");
        assert_eq!(err.code, "invalid_hookpoint");
        assert_eq!(err.class, ErrorClass::Contract);
        assert_eq!(err.phase, ErrorPhase::Validate);
    }

    #[test]
    fn rejects_out_of_scope_hookpoint() {
        // v3 只对 responses 族有定义；越界传入 fail closed。
        let mut headers = base_headers();
        headers.remove(SESSION_ID_HEADER);
        headers.remove(THREAD_ID_HEADER);
        headers.remove(WINDOW_ID_HEADER);
        headers.insert(HOOKPOINT_HEADER, HeaderValue::from_static("v3"));
        let err = parse_request_context(&headers, Operation::Models).expect_err("must fail");
        assert_eq!(err.code, "invalid_hookpoint_scope");
        // 同一 operation 传 v1 合法（v1 是默认实现，处处成立）。
        let mut headers = base_headers();
        headers.remove(SESSION_ID_HEADER);
        headers.remove(THREAD_ID_HEADER);
        headers.remove(WINDOW_ID_HEADER);
        headers.insert(HOOKPOINT_HEADER, HeaderValue::from_static("v1"));
        let ctx = parse_request_context(&headers, Operation::Models).expect("parse");
        assert_eq!(ctx.hookpoint, Hookpoint::V1);
    }

    #[test]
    fn models_operation_does_not_require_session() {
        let mut headers = base_headers();
        headers.remove(SESSION_ID_HEADER);
        headers.remove(THREAD_ID_HEADER);
        headers.remove(WINDOW_ID_HEADER);
        let ctx = parse_request_context(&headers, Operation::Models).expect("parse");
        assert!(ctx.session.is_none());
    }

    #[test]
    fn user_id_is_optional_and_never_fails_parse() {
        // 缺省 None：不注入 = 不抓（G4 零开销路径）。
        let ctx = parse_request_context(&base_headers(), Operation::Responses).expect("parse");
        assert!(ctx.user_id.is_none());
        // 注入后按 optional 模式解析（capture-r1）。
        let mut headers = base_headers();
        headers.insert(USER_ID_HEADER, HeaderValue::from_static("3"));
        let ctx = parse_request_context(&headers, Operation::Responses).expect("parse");
        assert_eq!(ctx.user_id.as_deref(), Some("3"));
    }

    #[test]
    fn secret_token_never_leaks_in_debug() {
        let ctx = parse_request_context(&base_headers(), Operation::Responses).expect("parse");
        let debug = format!("{ctx:?}");
        assert!(!debug.contains("sk-secret"));
        assert!(debug.contains("<redacted>") || debug.contains("SecretString"));
    }

    #[test]
    fn socks_kinds_parse_but_are_rejected_later() {
        let mut headers = base_headers();
        headers.insert(PROXY_KIND_HEADER, HeaderValue::from_static("socks5h"));
        headers.insert(
            PROXY_URL_HEADER,
            HeaderValue::from_static("socks5h://proxy:1080"),
        );
        let ctx = parse_request_context(&headers, Operation::Responses).expect("parse");
        assert_eq!(ctx.proxy_kind, ProxyKind::Socks5h);
        assert!(ctx.proxy_url.is_some());
    }
}

#[cfg(test)]
mod p1_header_tests {
    use super::*;

    fn base() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(PROTOCOL_VERSION_HEADER, HeaderValue::from_static("1"));
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_p1"));
        headers.insert(ACCOUNT_KEY_HEADER, HeaderValue::from_static("ak"));
        headers.insert(CREDENTIAL_REVISION_HEADER, HeaderValue::from_static("rev"));
        headers.insert(ACCESS_TOKEN_HEADER, HeaderValue::from_static("sk-token"));
        headers.insert(PROXY_KIND_HEADER, HeaderValue::from_static("direct"));
        headers
    }

    #[test]
    fn refresh_token_parsed_as_secret_and_redacted() {
        let mut headers = base();
        headers.insert(
            REFRESH_TOKEN_HEADER,
            HeaderValue::from_static("rt-secret-xyz"),
        );
        headers.insert(
            AGENT_RUNTIME_ID_HEADER,
            HeaderValue::from_static("runtime-1"),
        );
        let ctx = parse_request_context(&headers, Operation::OAuthRefresh).expect("parse");
        assert_eq!(
            ctx.refresh_token.as_ref().expect("token").as_str(),
            "rt-secret-xyz"
        );
        let debug = format!("{ctx:?}");
        assert!(
            !debug.contains("rt-secret-xyz"),
            "refresh token must be redacted"
        );
        assert_eq!(ctx.agent_runtime_id.as_deref(), Some("runtime-1"));
    }

    #[test]
    fn p1_headers_never_leak_via_debug() {
        let mut headers = base();
        headers.insert(
            AGENT_PRIVATE_KEY_HEADER,
            HeaderValue::from_static("pk-super-secret"),
        );
        let ctx = parse_request_context(&headers, Operation::OAuthRefresh).expect("parse");
        let debug = format!("{ctx:?}");
        assert!(!debug.contains("pk-super-secret"));
        assert!(!debug.contains("rt-"));
    }
}

#[cfg(test)]
mod carried_header_tests {
    use super::*;
    use crate::contract::operation::Operation;

    fn base() -> HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(PROTOCOL_VERSION_HEADER, HeaderValue::from_static("1"));
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_carried"));
        headers.insert(ACCOUNT_KEY_HEADER, HeaderValue::from_static("ak-1"));
        headers.insert(
            CREDENTIAL_REVISION_HEADER,
            HeaderValue::from_static("rev-1"),
        );
        headers.insert(ACCESS_TOKEN_HEADER, HeaderValue::from_static("sk-secret"));
        headers.insert(PROXY_KIND_HEADER, HeaderValue::from_static("direct"));
        headers.insert(
            SESSION_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000001"),
        );
        headers.insert(
            THREAD_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002"),
        );
        headers.insert(
            WINDOW_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002:1"),
        );
        headers
    }

    fn with_carried(raw: &str) -> Result<RequestContext, LocalError> {
        let mut headers = base();
        headers.insert(
            CARRIED_HEADERS_HEADER,
            HeaderValue::from_str(raw).expect("header value"),
        );
        parse_request_context(&headers, Operation::Responses)
    }

    #[test]
    fn absent_carrier_is_empty_and_never_fails() {
        // 旧版 Go（无承载通道）必须继续可用：缺省 = 空 map，不 fail。
        let ctx = parse_request_context(&base(), Operation::Responses).expect("parse");
        assert!(ctx.carried_headers.is_empty());
        assert!(ctx.carried_header_map().is_empty());
    }

    #[test]
    fn whitelisted_values_are_carried_and_injected_verbatim() {
        let raw = r#"{"x-codex-turn-metadata":"{\"sandbox\":\"seatbelt\",\"model\":\"gpt-5\"}","user-agent":"codex-tui/0.159.0 (Ubuntu 22.4.0; x86_64) xterm-256color","openai-beta":"responses_multi_agent=2026-01-01"}"#;
        let ctx = with_carried(raw).expect("parse");
        assert_eq!(ctx.carried_headers.len(), 3);
        let outbound = ctx.carried_header_map();
        assert_eq!(
            outbound
                .get("x-codex-turn-metadata")
                .expect("turn metadata"),
            r#"{"sandbox":"seatbelt","model":"gpt-5"}"#
        );
        assert_eq!(
            outbound.get("user-agent").unwrap(),
            "codex-tui/0.159.0 (Ubuntu 22.4.0; x86_64) xterm-256color"
        );
        assert_eq!(
            outbound.get("openai-beta").unwrap(),
            "responses_multi_agent=2026-01-01"
        );
    }

    #[test]
    fn credentials_are_never_carried() {
        // 硬护栏：authorization / chatgpt-account-id / cookie 出现即 fail closed
        // （不静默丢弃，避免掩盖上游身份注入缺陷）。
        for denied in CARRIED_HEADER_DENYLIST {
            let raw = format!(r#"{{"{denied}":"secret-value"}}"#);
            let err = with_carried(&raw).expect_err("credential must be rejected");
            assert_eq!(err.code, "carried_headers_credential_rejected");
            assert!(
                !format!("{err:?}").contains("secret-value"),
                "凭据值绝不出现在错误正文"
            );
        }
    }

    #[test]
    fn non_whitelisted_headers_are_dropped() {
        // version 不在白名单：官方 provider 自有，承载它会造出 R-Go 没有的头。
        // x-client-request-id 相反——它在白名单里（官方不变式 == session-id，
        // R-Go 会发），必须被保留。
        let raw = r#"{"version":"0.159.0","x-client-request-id":"0198f2b0-0000-7000-8000-000000000001","x-codex-window-id":"0198f2b0-0000-7000-8000-000000000002:1"}"#;
        let ctx = with_carried(raw).expect("parse");
        assert_eq!(ctx.carried_headers.len(), 2);
        assert_eq!(
            ctx.carried_headers
                .get("x-codex-window-id")
                .map(String::as_str),
            Some("0198f2b0-0000-7000-8000-000000000002:1")
        );
        let outbound = ctx.carried_header_map();
        assert!(outbound.get("version").is_none());
        assert_eq!(
            outbound.get("x-client-request-id").expect("carried"),
            "0198f2b0-0000-7000-8000-000000000001"
        );
    }

    /// ★ `x-codex-installation-id` 是**消费型**白名单项：承载通道必须收下它
    /// （见 [`crate::transport::metadata::effective_installation_id`]），与
    /// `version` 这类"白名单外键"语义**相反**——后者解析期即被丢弃。
    #[test]
    fn carried_installation_id_is_accepted_by_the_v1_channel() {
        let raw = r#"{"x-codex-installation-id":"0442ca22-065d-4351-bb85-aacd0b38faaf"}"#;
        let ctx = with_carried(raw).expect("parse");
        assert_eq!(
            ctx.carried_headers
                .get("x-codex-installation-id")
                .map(String::as_str),
            Some("0442ca22-065d-4351-bb85-aacd0b38faaf")
        );
        assert!(CARRIED_HEADER_NAMES.contains(&"x-codex-installation-id"));
        assert_eq!(CARRIED_INSTALLATION_ID_KEY, "x-codex-installation-id");
    }

    #[test]
    fn malformed_carrier_is_contract_error() {
        for raw in ["not-json", "[]", r#"{"x-client-request-id":1}"#] {
            let err = with_carried(raw).expect_err("malformed");
            assert_eq!(err.code, "invalid_carried_headers");
        }
    }

    #[test]
    fn blank_carrier_is_empty() {
        let ctx = with_carried("   ").expect("parse");
        assert!(ctx.carried_headers.is_empty());
    }
}

#[cfg(test)]
mod v3_outbound_header_tests {
    use super::*;

    fn base() -> HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(PROTOCOL_VERSION_HEADER, HeaderValue::from_static("1"));
        headers.insert(REQUEST_ID_HEADER, HeaderValue::from_static("req_v3"));
        headers.insert(ACCOUNT_KEY_HEADER, HeaderValue::from_static("ak-1"));
        headers.insert(
            CREDENTIAL_REVISION_HEADER,
            HeaderValue::from_static("rev-1"),
        );
        headers.insert(ACCESS_TOKEN_HEADER, HeaderValue::from_static("sk-secret"));
        headers.insert(PROXY_KIND_HEADER, HeaderValue::from_static("direct"));
        headers.insert(
            SESSION_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000001"),
        );
        headers.insert(
            THREAD_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002"),
        );
        headers.insert(
            WINDOW_ID_HEADER,
            HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002:1"),
        );
        headers
    }

    /// 官方 159.2 responses 出站头集的合法子集（含白名单内与白名单外两类）。
    const OUTBOUND_JSON: &str = r#"{"version":"0.159.2","accept":"text/event-stream","content-type":"application/json","user-agent":"codex-tui/0.159.2 (Mac OS 15.5.0; arm64)","host":"chatgpt.com","content-length":"481","accept-encoding":"gzip","x-codex-installation-id":"inst-1"}"#;

    fn with_hookpoint(
        hookpoint: Option<&str>,
        outbound: &str,
    ) -> Result<RequestContext, LocalError> {
        with_hookpoint_op(hookpoint, outbound, Operation::Responses)
    }

    fn with_hookpoint_op(
        hookpoint: Option<&str>,
        outbound: &str,
        operation: Operation,
    ) -> Result<RequestContext, LocalError> {
        let mut headers = base();
        if let Some(raw) = hookpoint {
            headers.insert(HOOKPOINT_HEADER, HeaderValue::from_str(raw).expect("value"));
        }
        headers.insert(
            OUTBOUND_HEADERS_HEADER,
            HeaderValue::from_str(outbound).expect("value"),
        );
        parse_request_context(&headers, operation)
    }

    #[test]
    fn v3_parses_outbound_carrier_and_filters_to_allowlist() {
        let ctx = with_hookpoint(Some("v3"), OUTBOUND_JSON).expect("parse");
        assert_eq!(ctx.hookpoint, Hookpoint::V3);

        let outbound = ctx.outbound_header_map();
        for kept in ["version", "accept", "content-type", "user-agent"] {
            assert!(outbound.get(kept).is_some(), "{kept} 必须在 v3 出站集内");
        }
        // ctd-owned 传输头与白名单外头在结构上不可能进入（R5）；
        // `x-codex-installation-id` 是 **compact 专属**头，`/responses` 下与
        // 白名单外键同语义（静默丢弃），见
        // `compact_extra_header_is_scoped_to_compact_operation`。
        for dropped in [
            "host",
            "content-length",
            "accept-encoding",
            "x-codex-installation-id",
        ] {
            assert!(
                outbound.get(dropped).is_none(),
                "{dropped} 不得进入 v3 出站集"
            );
        }
        // 值原样，不做归一化。
        assert_eq!(outbound.get("version").expect("version"), "0.159.2");
    }

    /// ★ operation 收窄（白名单）：`x-codex-installation-id` **只在 compact**
    /// 被放行；同一份交付在 `/responses` 下与白名单外键同语义（静默丢弃）。
    ///
    /// 这是"`/responses` 不得多出该头"的第一道（也是生产路径上的唯一一道）
    /// 防线——第二道在 `responses_v3::order_v3_outbound_headers` 的表外追加分支。
    #[test]
    fn compact_extra_header_is_scoped_to_compact_operation() {
        let raw = r#"{"accept":"application/json","content-type":"application/json","x-codex-installation-id":"inst-1"}"#;

        let compact =
            with_hookpoint_op(Some("v3"), raw, Operation::ResponsesCompact).expect("parse");
        assert_eq!(
            compact
                .outbound_headers
                .get("x-codex-installation-id")
                .map(String::as_str),
            Some("inst-1")
        );
        // 白名单内两项 + compact 例外一项。
        assert_eq!(compact.outbound_headers.len(), 3);

        let responses = with_hookpoint_op(Some("v3"), raw, Operation::Responses).expect("parse");
        assert!(
            !responses
                .outbound_headers
                .contains_key("x-codex-installation-id")
        );
        assert_eq!(responses.outbound_headers.len(), 2);
    }

    /// compact 例外表与 responses 白名单表必须**不相交**：一旦把
    /// `x-codex-installation-id` 并进 [`V3_HEADER_ALLOWLIST`]，`/responses`
    /// 就会多出官方 18 头里没有的名字（且与 Go 侧 `codexV3HeaderNames` 的
    /// 逐字相等约定失配）。
    #[test]
    fn compact_extras_stay_out_of_the_responses_allowlist() {
        assert!(!V3_COMPACT_EXTRA_HEADER_NAMES.is_empty());
        for name in V3_COMPACT_EXTRA_HEADER_NAMES {
            assert!(
                !V3_HEADER_ALLOWLIST.contains(name),
                "{name} 不得并入 /responses 白名单"
            );
            assert!(
                !CARRIED_HEADER_DENYLIST.contains(name),
                "{name} 不得同时是凭据护栏"
            );
        }
    }

    #[test]
    fn v1_ignores_the_v3_carrier_verbatim() {
        // ★ v1 逐字不变：即使（错误地）携带 v3 通道，v1 的解析结果也必须与
        // "没有 v3 实现之前"完全一致——不解析、不报错、不改变任何字段。
        let absent = parse_request_context(&base(), Operation::Responses).expect("parse");
        let v1_with_carrier = with_hookpoint(Some("v1"), OUTBOUND_JSON).expect("parse");
        let implicit_v1_with_carrier = with_hookpoint(None, OUTBOUND_JSON).expect("parse");

        for ctx in [&v1_with_carrier, &implicit_v1_with_carrier] {
            assert_eq!(ctx.hookpoint, Hookpoint::V1);
            assert!(ctx.outbound_headers.is_empty(), "v1 不得解析 v3 通道");
            assert!(ctx.outbound_header_map().is_empty());
            assert_eq!(ctx.request_id, absent.request_id);
            assert_eq!(ctx.account_key, absent.account_key);
            assert_eq!(ctx.credential_revision, absent.credential_revision);
            assert_eq!(ctx.proxy_kind, absent.proxy_kind);
            assert_eq!(ctx.purpose, absent.purpose);
            assert_eq!(ctx.session, absent.session);
            assert_eq!(ctx.carried_headers, absent.carried_headers);
            assert_eq!(ctx.deadline.is_some(), absent.deadline.is_some());
            assert_eq!(ctx.admission_timeout_ms, absent.admission_timeout_ms);
        }

        // 连"非法 v3 通道"在 v1 下也不得报错（v1 完全不看它）。
        with_hookpoint(Some("v1"), "not-json").expect("v1 不解析该通道");
    }

    #[test]
    fn v3_carrier_absent_is_empty_and_never_fails() {
        // Go 尚未升级（不发该头）时 v3 仍可解析：出站集为空，由必需头校验
        // 在发出前 fail closed（而不是解析期）。
        let mut headers = base();
        headers.insert(HOOKPOINT_HEADER, HeaderValue::from_static("v3"));
        let ctx = parse_request_context(&headers, Operation::Responses).expect("parse");
        assert_eq!(ctx.hookpoint, Hookpoint::V3);
        assert!(ctx.outbound_headers.is_empty());
    }

    #[test]
    fn v3_carrier_rejects_credentials_fail_closed() {
        // 硬护栏在 v3 通道上不得放宽（P8）：出现即 typed error，且绝不回显值。
        for denied in CARRIED_HEADER_DENYLIST {
            let raw = format!(r#"{{"{denied}":"secret-value"}}"#);
            let err = with_hookpoint(Some("v3"), &raw).expect_err("凭据头必须 fail closed");
            assert_eq!(err.code, "outbound_headers_credential_rejected", "{denied}");
            assert_eq!(err.class, ErrorClass::Contract);
            assert_eq!(err.phase, ErrorPhase::Validate);
            assert!(!format!("{err:?}").contains("secret-value"));
        }
    }

    #[test]
    fn v3_carrier_malformed_is_contract_error() {
        for raw in ["not-json", "[]", r#"{"accept":1}"#] {
            let err = with_hookpoint(Some("v3"), raw).expect_err("malformed");
            assert_eq!(err.code, "invalid_outbound_headers", "{raw}");
        }
        // 空白视为"不承载"（与承载通道同语义）。
        let ctx = with_hookpoint(Some("v3"), "   ").expect("parse");
        assert!(ctx.outbound_headers.is_empty());
    }

    #[test]
    fn v3_allowlist_excludes_ctd_owned_and_credentials() {
        for name in [
            "host",
            "content-length",
            "accept-encoding",
            "authorization",
            "chatgpt-account-id",
            "cookie",
        ] {
            assert!(
                !V3_HEADER_ALLOWLIST.contains(&name),
                "{name} 不得进入 V3_HEADER_ALLOWLIST"
            );
        }
        // 必需头必须都在白名单内（否则"必需"永远无法满足）。
        for name in V3_REQUIRED_HEADERS {
            assert!(V3_HEADER_ALLOWLIST.contains(name), "{name} 必须在白名单内");
        }
        // 审计头必须都在白名单内。
        for name in V3_AUDIT_HEADERS {
            assert!(V3_HEADER_ALLOWLIST.contains(name), "{name} 必须在白名单内");
        }
        // v3 白名单 ⊇ v1 承载白名单（v3 是全集，v1 是子集），两个例外：
        //  - `openai-beta`：官方 HTTP POST 不发（R-Go 的 HTTP 策略同样剥离）；
        //  - `x-codex-installation-id`：官方 `/responses` **不作为头**发（账号隔离
        //    由 body `client_metadata` 表达）⇒ 它不在 `/responses` 的 18 头表里，
        //    只在 compact 下经 [`V3_COMPACT_EXTRA_HEADER_NAMES`] 放行；v1 侧则是
        //    承载通道的**消费型**项（projection 的 installation_id 来源）。
        for name in CARRIED_HEADER_NAMES {
            if *name == "openai-beta" {
                assert!(!V3_HEADER_ALLOWLIST.contains(name));
                continue;
            }
            if *name == CARRIED_INSTALLATION_ID_KEY {
                assert!(!V3_HEADER_ALLOWLIST.contains(name));
                assert!(V3_COMPACT_EXTRA_HEADER_NAMES.contains(name));
                continue;
            }
            assert!(
                V3_HEADER_ALLOWLIST.contains(name),
                "v1 承载的 {name} 必须仍被 v3 承载"
            );
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    fn ctx_with_deadline(value: &str) -> Result<RequestContext, LocalError> {
        let mut headers = http::HeaderMap::new();
        for (name, val) in [
            (PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION),
            (REQUEST_ID_HEADER, "r1"),
            (ACCOUNT_KEY_HEADER, "k1"),
            (CREDENTIAL_REVISION_HEADER, "rev1"),
            (ACCESS_TOKEN_HEADER, "tok"),
            (PROXY_KIND_HEADER, "direct"),
            (DEADLINE_UNIX_MS_HEADER, value),
        ] {
            headers.insert(name, val.parse().expect("header value"));
        }
        parse_request_context(&headers, crate::contract::operation::Operation::Models)
    }

    #[test]
    fn malformed_deadline_is_contract_error() {
        let err = ctx_with_deadline("not-a-number").expect_err("malformed");
        assert_eq!(err.code, "invalid_deadline");
        let err = ctx_with_deadline("0").expect_err("zero");
        assert_eq!(err.code, "invalid_deadline");
        let err = ctx_with_deadline("-5").expect_err("negative");
        assert_eq!(err.code, "invalid_deadline");
    }

    #[test]
    fn past_deadline_expires_immediately() {
        let past_ms = now_ms() - 5000;
        let ctx = ctx_with_deadline(&past_ms.to_string()).expect("past deadline parses");
        let deadline = ctx.deadline.expect("deadline present");
        assert!(
            deadline <= Instant::now() + std::time::Duration::from_millis(100),
            "已过 deadline 必须立即过期"
        );
    }

    #[test]
    fn invalid_admission_timeout_fails_closed() {
        for bad in ["abc", "0", "-5", "99999999999999999999", "1.5"] {
            let mut headers = http::HeaderMap::new();
            for (name, val) in [
                (PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION),
                (REQUEST_ID_HEADER, "r1"),
                (ACCOUNT_KEY_HEADER, "k1"),
                (CREDENTIAL_REVISION_HEADER, "rev1"),
                (ACCESS_TOKEN_HEADER, "tok"),
                (PROXY_KIND_HEADER, "direct"),
                (ADMISSION_TIMEOUT_MS_HEADER, bad),
            ] {
                headers.insert(name, val.parse().expect("header value"));
            }
            let err =
                parse_request_context(&headers, crate::contract::operation::Operation::Models)
                    .expect_err(bad);
            assert_eq!(err.code, "invalid_admission_timeout", "{}", bad);
        }
        // 合法值通过。
        let mut headers = http::HeaderMap::new();
        for (name, val) in [
            (PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION),
            (REQUEST_ID_HEADER, "r1"),
            (ACCOUNT_KEY_HEADER, "k1"),
            (CREDENTIAL_REVISION_HEADER, "rev1"),
            (ACCESS_TOKEN_HEADER, "tok"),
            (PROXY_KIND_HEADER, "direct"),
            (ADMISSION_TIMEOUT_MS_HEADER, "100"),
        ] {
            headers.insert(name, val.parse().expect("header value"));
        }
        let ctx = parse_request_context(&headers, crate::contract::operation::Operation::Models)
            .expect("valid");
        assert_eq!(ctx.admission_timeout_ms, Some(100));
    }

    #[test]
    fn missing_deadline_defaults_to_none() {
        let mut headers = http::HeaderMap::new();
        for (name, val) in [
            (PROTOCOL_VERSION_HEADER, PROTOCOL_VERSION),
            (REQUEST_ID_HEADER, "r1"),
            (ACCOUNT_KEY_HEADER, "k1"),
            (CREDENTIAL_REVISION_HEADER, "rev1"),
            (ACCESS_TOKEN_HEADER, "tok"),
            (PROXY_KIND_HEADER, "direct"),
        ] {
            headers.insert(name, val.parse().expect("header value"));
        }
        let ctx = parse_request_context(&headers, crate::contract::operation::Operation::Models)
            .expect("parsed");
        assert!(ctx.deadline.is_none());
    }

    #[test]
    fn future_deadline_kept() {
        let future_ms = now_ms() + 60_000;
        let ctx = ctx_with_deadline(&future_ms.to_string()).expect("future deadline");
        assert!(ctx.deadline.is_some());
    }
}
