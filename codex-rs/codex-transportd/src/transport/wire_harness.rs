//! 出站线上的 mock 上游读取工具（仅测试构建）。
//!
//! 被 v1（[`crate::transport::compact`]）与 v3（[`crate::transport::responses_v3`]）
//! 的出站形态测试共用：真实 `reqwest` 传输 → 本地 `TcpListener` 记录原始字节
//! → 断言头名序列 / 头值 / `content-length`。放在一处是为了让两侧对"线上字节"
//! 的解读方式逐字一致（同一套小写归一与解析规则），避免 v1/v3 的断言口径漂移。

use std::io::Read;

/// 读一个完整请求（head + `content-length` body），返回 `(head, body)`。
pub(crate) fn read_request(sock: &mut std::net::TcpStream) -> (String, Vec<u8>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = sock.read(&mut chunk).expect("read request head");
        if n == 0 {
            panic!("mock upstream: connection closed before head");
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        assert!(buf.len() < 1024 * 1024, "request head exceeds bound");
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let content_length = head
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or_else(|| panic!("mock upstream: request has no content-length: {head}"));
    while buf.len() < head_end + content_length {
        let n = sock.read(&mut chunk).expect("read request body");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    (head, buf[head_end..].to_vec())
}

/// 从原始 head 里取头名序列（小写、按 wire 顺序）。
pub(crate) fn head_header_names(head: &str) -> Vec<String> {
    head.split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, _)| name.trim().to_ascii_lowercase())
        .collect()
}

/// 取某个头的值（大小写不敏感；head 内同名的第一个）。
pub(crate) fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.split("\r\n")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}
