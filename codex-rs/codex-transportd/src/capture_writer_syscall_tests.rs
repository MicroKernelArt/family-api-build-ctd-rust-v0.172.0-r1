//! write_all / fd helper 单测（capture_writer 模块；AM4）。
//!
//! 注：write(2)==0（非空输入）在常规 fd 上无法确定性构造，其 WriteZero
//! 映射由静态 gate 覆盖（patches/transportd/tools/static-fix-gates.sh）；
//! 此处覆盖循环语义与错误传播。

use super::*;

fn open_temp_file(dir: &tempfile::TempDir, name: &str, flags: i32) -> OwnedFd {
    let c = cstr(dir.path().join(name).to_str().unwrap()).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), flags, 0o600) };
    assert!(fd >= 0, "open failed: {}", io::Error::last_os_error());
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn read_file(dir: &tempfile::TempDir, name: &str) -> Vec<u8> {
    let fd = open_temp_file(dir, name, libc::O_RDONLY | libc::O_CLOEXEC);
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert!(n >= 0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    out
}

#[test]
fn write_all_writes_bytes_exactly() {
    let dir = tempfile::tempdir().expect("tempdir");
    {
        let fd = open_temp_file(
            &dir,
            "w",
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
        );
        write_all(fd.as_raw_fd(), &vec![7u8; 100_000]).expect("write_all ok");
        write_all(fd.as_raw_fd(), b"tail").expect("append ok");
    }
    let mut want = vec![7u8; 100_000];
    want.extend_from_slice(b"tail");
    assert_eq!(read_file(&dir, "w"), want);
}

#[test]
fn write_all_accepts_empty_input_without_syscall() {
    // 空输入必须直接 Ok（不进入循环，也就不存在 0 字节写歧义）。
    let dir = tempfile::tempdir().expect("tempdir");
    let fd = open_temp_file(
        &dir,
        "w",
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
    );
    write_all(fd.as_raw_fd(), b"").expect("empty write is a no-op Ok");
}

#[test]
fn write_all_propagates_os_error_without_retry() {
    // O_RDONLY fd 上 write → EBADF：错误必须原样传播（计数在调用方）。
    // O_CREAT 保证打开成功（测试对象是 read-only fd 上的 write 语义）。
    let dir = tempfile::tempdir().expect("tempdir");
    let fd = open_temp_file(&dir, "r", libc::O_RDONLY | libc::O_CREAT | libc::O_CLOEXEC);
    let err = write_all(fd.as_raw_fd(), b"data").expect_err("EBADF must surface");
    assert_eq!(err.raw_os_error(), Some(libc::EBADF));
}

#[test]
fn rename_at_moves_within_dir_and_errors_across_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = crate::capture_writer::open_root(dir.path()).expect("root");
    let fd = create_file_at(root.as_raw_fd(), "from").expect("create");
    drop(fd);
    rename_at(root.as_raw_fd(), "from", "to").expect("rename");
    assert!(!dir.path().join("from").exists());
    assert!(dir.path().join("to").exists());
    let err = rename_at(root.as_raw_fd(), "missing", "x").expect_err("rename of missing");
    assert_eq!(err.kind(), io::ErrorKind::NotFound);
}

#[test]
fn create_dir_at_rejects_symlink_and_plain_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = crate::capture_writer::open_root(dir.path()).expect("root");
    let d = create_dir_at(root.as_raw_fd(), "d").expect("mkdir");
    drop(d);
    // 已存在真目录：幂等成功。
    assert!(create_dir_at(root.as_raw_fd(), "d").is_ok());
    // 预植入普通文件同名 → 拒绝（O_DIRECTORY 打开失败）。
    {
        let f = create_file_at(root.as_raw_fd(), "f").expect("file");
        drop(f);
    }
    assert!(create_dir_at(root.as_raw_fd(), "f").is_err());
    // 预植入 symlink 同名 → O_NOFOLLOW 一律拒绝（防穿越，G3）。
    assert!(std::os::unix::fs::symlink("../escape", dir.path().join("l")).is_ok());
    assert!(create_dir_at(root.as_raw_fd(), "l").is_err());
}
