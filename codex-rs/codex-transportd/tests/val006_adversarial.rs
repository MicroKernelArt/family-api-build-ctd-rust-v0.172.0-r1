//! VAL-006 adversarial fixture 表（TLS-CTD-001 T-003 / §16.2 / INV-006）。
//!
//! 任意 URL/path/method/Host/header/redirect 输入必须在 admission 阶段本地
//! 拒绝，且零 DNS/proxy/upstream I/O。本测试只做纯函数断言（无网络），
//! 并通过断言错误 code 证明失败发生在任何 I/O 之前。

use codex_transportd::contract::headers::parse_request_context;
use codex_transportd::contract::operation::Operation;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;

fn base_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-ctd-protocol-version", HeaderValue::from_static("1"));
    headers.insert("x-ctd-request-id", HeaderValue::from_static("req_val006"));
    headers.insert("x-ctd-account-key", HeaderValue::from_static("ak"));
    headers.insert("x-ctd-credential-revision", HeaderValue::from_static("rev"));
    headers.insert("x-ctd-access-token", HeaderValue::from_static("sk-token"));
    headers.insert("x-ctd-proxy-kind", HeaderValue::from_static("direct"));
    headers.insert(
        "x-ctd-session-id",
        HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000001"),
    );
    headers.insert(
        "x-ctd-thread-id",
        HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002"),
    );
    headers.insert(
        "x-ctd-window-id",
        HeaderValue::from_static("0198f2b0-0000-7000-8000-000000000002:1"),
    );
    headers
}

/// 任意 URL/path/method/Host：内部 path 映射之外的组合一律 None。
#[test]
fn val006_arbitrary_url_path_method_rejected() {
    let cases: &[(&str, &str)] = &[
        ("POST", "/_internal/v1/proxy?url=https://evil.example"),
        ("GET", "/_internal/v1/responses"),
        ("POST", "/_internal/v1/models"),
        ("DELETE", "/_internal/v1/responses"),
        ("POST", "/_internal/v2/responses"),
        ("POST", "/_internal/v1/responses/ws"),
        ("POST", "/_internal/v1//../etc/passwd"),
        ("GET", "/"),
        ("POST", "/_internal/v1/responses/../compact"),
    ];
    for (method, path) in cases {
        let m: Method = method.parse().unwrap();
        let op = Operation::from_internal_path(&m, path);
        assert!(
            op.is_none() || op == Some(Operation::ResponsesWebSocketV2),
            "path {path} ({method}) must not map to an HTTP operation"
        );
    }
    // 合法组合必须映射。
    assert_eq!(
        Operation::from_internal_path(&Method::POST, "/_internal/v1/responses"),
        Some(Operation::Responses)
    );
}

/// 任意 header：未知 X-CTD-* 或 Host 覆盖不进入请求上下文（只解析已知集）。
#[test]
fn val006_arbitrary_headers_ignored_and_host_never_accepted() {
    let mut headers = base_headers();
    headers.insert(
        "x-ctd-upstream-host",
        HeaderValue::from_static("evil.example"),
    );
    headers.insert("host", HeaderValue::from_static("evil.example"));
    headers.insert("x-ctd-arbitrary-header", HeaderValue::from_static("x"));
    let ctx = parse_request_context(&headers, Operation::Responses).expect("parse");
    // 请求上下文只携带契约字段；任意 header 无处表达（§10.3 封闭请求）。
    assert_eq!(ctx.proxy_kind.as_str(), "direct");
}

/// 错误协议版本 / 缺必需 header / 非法 session：admission 拒绝。
#[test]
fn val006_protocol_and_shape_validation() {
    let mut headers = base_headers();
    headers.insert("x-ctd-protocol-version", HeaderValue::from_static("2"));
    let err = parse_request_context(&headers, Operation::Responses).expect_err("v2");
    assert_eq!(err.code, "invalid_protocol_version");

    let mut headers = base_headers();
    headers.remove("x-ctd-access-token");
    let err = parse_request_context(&headers, Operation::Responses).expect_err("no token");
    assert_eq!(err.code, "missing_required_header");

    let mut headers = base_headers();
    headers.insert("x-ctd-window-id", HeaderValue::from_static("../etc"));
    let err = parse_request_context(&headers, Operation::Responses).expect_err("bad window");
    assert_eq!(err.code, "invalid_session");

    let mut headers = base_headers();
    headers.insert("x-ctd-proxy-kind", HeaderValue::from_static("gopher"));
    let err = parse_request_context(&headers, Operation::Responses).expect_err("bad kind");
    assert_eq!(err.code, "invalid_proxy_kind");
}

/// SOCKS fail closed（FR-017）：未支持时不宣告、不回退 direct。
#[test]
fn val006_socks_fails_closed() {
    use codex_transportd::redaction::SecretString;
    use codex_transportd::transport::proxy::resolve_route;
    let err = resolve_route(
        codex_transportd::contract::ProxyKind::Socks5h,
        Some(&SecretString::from("socks5h://proxy:1080")),
    )
    .expect_err("socks fail closed");
    assert_eq!(err.code, "unsupported_proxy_kind");
}

/// secret 永不进入 Debug/错误正文（FR-022）。
#[test]
fn val006_secrets_never_leak() {
    let ctx = parse_request_context(&base_headers(), Operation::Responses).expect("parse");
    let debug = format!("{ctx:?}");
    assert!(!debug.contains("sk-token"));
    let err = codex_transportd::contract::error::codes::invalid_session();
    let body = serde_json::to_string(&err.problem_body("req")).unwrap();
    assert!(!body.to_lowercase().contains("token"));
}

/// 重定向：Go 侧 CheckRedirect 拒绝（http.ErrUseLastResponse）——
/// 该行为在 Go 单测覆盖；此处校验 ops 契约清单（R2-codex 显式解析）。
///
/// fixture 解析契约（R2-codex / F-0001）：本测试只在显式提供跨仓上下文
/// （`CTD_OPS_FIXTURE_MANIFEST` + `CTD_OPS_ROOT`）时执行；一旦执行，
/// 上下文缺失/身份不符一律失败（fail closed），绝无静默跳过。默认套件
/// 通过 `#[ignore]` 显式排除（清洁拓扑由 ops_fixture 单元负向测试证明
/// fail-closed 行为）；三仓拓扑验证阶段以 `--ignored` 运行本测试。
#[test]
#[ignore = "requires explicit cross-repo context (CTD_OPS_FIXTURE_MANIFEST + CTD_OPS_ROOT); run with --ignored in the three-repo topology"]
fn val006_ops_contract_inventory_identity_and_redirect_policy() {
    let fixture = codex_transportd::ops_fixture::resolve_from_env(
        codex_transportd::ops_fixture::ROLE_OPS_CONTRACT_INVENTORY,
    )
    .unwrap_or_else(|e| panic!("ops fixture context invalid (fail closed): {e}"));
    let raw = std::fs::read_to_string(&fixture.path)
        .unwrap_or_else(|e| panic!("ops fixture {} unreadable: {e}", fixture.path.display()));
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("ops fixture {} invalid json: {e}", fixture.path.display()));
    let forbidden = parsed["forbidden_paths"]
        .as_array()
        .unwrap_or_else(|| panic!("ops fixture missing forbidden_paths"));
    assert!(
        forbidden.len() >= 4,
        "ops inventory must declare the forbidden path set"
    );
    // Rust 侧全量表必须覆盖 ops 清单声明的全部 internal 路径（身份对齐）。
    let ops_listed: Vec<&str> = parsed["operations"]
        .as_array()
        .unwrap_or_else(|| panic!("ops fixture missing operations"))
        .iter()
        .map(|o| o["internal_path"].as_str().expect("internal_path"))
        .collect();
    for path in ops_listed {
        let method = parsed["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["internal_path"].as_str() == Some(path))
            .and_then(|o| o["method"].as_str())
            .expect("method");
        let m: http::Method = method.parse().expect("valid method");
        assert_eq!(
            Operation::from_internal_path(&m, path).map(|op| op.internal_endpoint()),
            Some(path),
            "ops inventory path {path} must exist in the Rust closed set"
        );
    }
}
