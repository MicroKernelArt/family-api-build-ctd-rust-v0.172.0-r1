//! sanitize / 配置解析测试（capture 模块；兄弟文件，AM4 测试文件纪律）。

use super::*;

#[test]
fn sanitize_rejects_attack_samples() {
    for bad in [
        "../../etc",
        "a/b",
        "a\\0b",
        "a\0b",
        "",
        "a".repeat(65).as_str(),
        "用 户",
        "..",
        ".",
        "a b",
        "café",
    ] {
        assert!(sanitize(bad).is_none(), "must reject {bad:?}");
    }
    for good in ["abc-_09", "0198f2b0-0000-7000-8000-000000000001", "3"] {
        assert_eq!(sanitize(good), Some(good.to_string()));
    }
}

#[test]
fn config_from_env_requires_root() {
    // 不依赖进程 env 的纯逻辑：空 root = 关闭。
    // from_env 本身读进程 env；这里只覆盖 sanitize/queue 的纯逻辑面。
    assert!(sanitize("").is_none());
}

#[test]
fn queue_config_is_bounded_and_total_bytes_default() {
    // F2/R2-F3：队列配置有硬上限；非法/越界输入 fail 到缺省值而非 panic。
    assert_eq!(parse_queue(None), DEFAULT_QUEUE);
    assert_eq!(parse_queue(Some("")), DEFAULT_QUEUE);
    assert_eq!(parse_queue(Some("junk")), DEFAULT_QUEUE);
    assert_eq!(parse_queue(Some("0")), DEFAULT_QUEUE);
    assert_eq!(parse_queue(Some(" 8 ")), 8);
    // 可解析但超上限 → 硬封顶（F2）。
    assert_eq!(parse_queue(Some("99999999")), MAX_QUEUE);
    assert_eq!(parse_queue(Some(&MAX_QUEUE.to_string())), MAX_QUEUE);
    // u64 溢出字面量也必须安全落到缺省值（usize 解析失败）。
    assert_eq!(parse_queue(Some("18446744073709551616")), DEFAULT_QUEUE);
    assert_eq!(parse_max_total_bytes(None), DEFAULT_MAX_TOTAL_BYTES);
    assert_eq!(parse_max_total_bytes(Some("0")), DEFAULT_MAX_TOTAL_BYTES);
    assert_eq!(parse_max_total_bytes(Some("nope")), DEFAULT_MAX_TOTAL_BYTES);
    assert_eq!(parse_max_total_bytes(Some("1024")), 1024);
}

#[test]
fn max_total_bytes_env_parse_is_clamped_to_hard_cap() {
    // R2-F3：字节预算硬上限存在；usize::MAX 级配置被封顶而非放行，
    // 且解析输出恒在 Capture::new 验证域内（env 归一化显式且被测试）。
    assert!(MAX_TOTAL_BYTES >= DEFAULT_MAX_TOTAL_BYTES);
    assert_eq!(
        parse_max_total_bytes(Some(&MAX_TOTAL_BYTES.to_string())),
        MAX_TOTAL_BYTES
    );
    assert_eq!(
        parse_max_total_bytes(Some("18446744073709551615")),
        MAX_TOTAL_BYTES,
        "usize::MAX env value must clamp to the hard cap"
    );
    for raw in [
        None,
        Some(""),
        Some("junk"),
        Some("0"),
        Some("1024"),
        Some("999999999999"),
        Some(&usize::MAX.to_string()),
    ] {
        let v = parse_max_total_bytes(raw);
        assert!(
            v > 0 && v <= MAX_TOTAL_BYTES,
            "parse output {v} out of bounds"
        );
    }
    for raw in [
        None,
        Some(""),
        Some("junk"),
        Some("0"),
        Some("8"),
        Some("99999999"),
        Some(&usize::MAX.to_string()),
    ] {
        let q = parse_queue(raw);
        assert!(
            q > 0 && q <= MAX_QUEUE,
            "queue parse output {q} out of bounds"
        );
    }
}

#[test]
fn capture_new_rejects_invalid_programmatic_config() {
    // R2-F3：程序化配置直接进 Capture::new 必须验证——queue 0 / 越上限、
    // 字节预算 0 / 越上限 → InvalidInput，绝不 panic、绝不静默越界；
    // 验证先于 root 打开（错误域与文件系统无关）。
    let metrics = Arc::new(Metrics::default());
    let cfg = |queue, max_total_bytes| CaptureConfig {
        root: PathBuf::from("/nonexistent-capture-root-r2"),
        whitelist: HashSet::new(),
        queue,
        max_total_bytes,
    };
    for (queue, max_total_bytes) in [
        (0, DEFAULT_MAX_TOTAL_BYTES),             // queue 0
        (MAX_QUEUE + 1, DEFAULT_MAX_TOTAL_BYTES), // queue 越上限
        (usize::MAX, DEFAULT_MAX_TOTAL_BYTES),
        (DEFAULT_QUEUE, 0),                   // 预算 0
        (DEFAULT_QUEUE, MAX_TOTAL_BYTES + 1), // 预算越上限
        (DEFAULT_QUEUE, usize::MAX),
    ] {
        // Capture 不实现 Debug：用 let-else 显式拆分，不施加 Debug 约束。
        let Err(err) = Capture::new(cfg(queue, max_total_bytes), Arc::clone(&metrics)) else {
            panic!(
                "invalid programmatic config must be rejected: queue={queue} max_total_bytes={max_total_bytes}"
            );
        };
        assert_eq!(
            err.kind(),
            io::ErrorKind::InvalidInput,
            "queue={queue} max_total_bytes={max_total_bytes}"
        );
    }
}

#[test]
fn capture_new_accepts_bounded_configs_and_defaults() {
    // 边界内配置（含上下边界与 env 缺省值）必须成功构造；writer 线程随
    // tx 全部关闭自行退出（blocking_recv → None），不泄漏。
    let dir = tempfile::tempdir().expect("tempdir");
    for (queue, max_total_bytes) in [
        (1, 1),
        (MAX_QUEUE, MAX_TOTAL_BYTES),
        (DEFAULT_QUEUE, DEFAULT_MAX_TOTAL_BYTES),
    ] {
        let cap = Capture::new(
            CaptureConfig {
                root: dir.path().to_path_buf(),
                whitelist: HashSet::new(),
                queue,
                max_total_bytes,
            },
            Arc::new(Metrics::default()),
        )
        .expect("bounded config must construct");
        assert_eq!(cap.dropped_total(), 0);
    }
}
