//! 运行时配置：全部来自显式参数/环境变量，缺失关键项立即失败。

use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// UDS socket 路径（仅 unix stream，禁止 TCP listener）。
    pub socket_path: PathBuf,
    /// CODEX_HOME（installation bootstrap 目标）。
    pub codex_home: PathBuf,
    /// 运行期 /info 覆盖的 image digest（空则用编译期 BASE_IMAGE_REF）。
    pub image_digest_override: Option<String>,
    /// 运行期 /info 覆盖的 build-proof digest（空则用编译期注入值）。
    pub build_proof_digest_override: Option<String>,
    /// socket 父目录期望 owner uid/gid（PS-M4）：由部署 init 准备，
    /// 启动时校验一致；缺省（本地开发）跳过 owner 比较，仅做 symlink/
    /// world-writable 检查。
    pub socket_dir_expected_uid: Option<u32>,
    pub socket_dir_expected_gid: Option<u32>,
}

impl Config {
    /// 进程入口：从 `std::env::args()` 解析（R1-codex：CLI 契约唯一入口）。
    pub fn from_env() -> Result<Self, String> {
        Self::parse_from(env::args().skip(1))
    }

    /// 显式参数解析（可测试契约）：`--socket` 必填；未知参数/缺值/非法
    /// uid+gid 立即失败；`--help` 打印 usage 后返回 `Err("help")`（进程以
    /// 0 退出）。部署准备 socket 目录与 UID 10001 属主（PS-M4），本进程
    /// 只验证、绝不 chown/chmod 部署资源。
    pub fn parse_from<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let args: Vec<String> = args.into_iter().collect();
        let mut socket_path: Option<PathBuf> = None;
        let mut codex_home: Option<PathBuf> = None;
        let mut image_digest_override: Option<String> = None;
        let mut build_proof_digest_override: Option<String> = None;
        let mut socket_dir_expected_uid: Option<u32> = None;
        let mut socket_dir_expected_gid: Option<u32> = None;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--socket" => {
                    i += 1;
                    let v = args.get(i).ok_or("--socket requires a value")?;
                    socket_path = Some(PathBuf::from(v));
                }
                "--codex-home" => {
                    i += 1;
                    let v = args.get(i).ok_or("--codex-home requires a value")?;
                    codex_home = Some(PathBuf::from(v));
                }
                "--image-digest" => {
                    i += 1;
                    let v = args.get(i).ok_or("--image-digest requires a value")?;
                    image_digest_override = Some(v.clone());
                }
                "--build-proof-digest" => {
                    i += 1;
                    let v = args.get(i).ok_or("--build-proof-digest requires a value")?;
                    build_proof_digest_override = Some(v.clone());
                }
                "--socket-dir-uid" => {
                    i += 1;
                    let v = args.get(i).ok_or("--socket-dir-uid requires a value")?;
                    socket_dir_expected_uid = Some(
                        v.parse()
                            .map_err(|_| format!("invalid --socket-dir-uid: {v}"))?,
                    );
                }
                "--socket-dir-gid" => {
                    i += 1;
                    let v = args.get(i).ok_or("--socket-dir-gid requires a value")?;
                    socket_dir_expected_gid = Some(
                        v.parse()
                            .map_err(|_| format!("invalid --socket-dir-gid: {v}"))?,
                    );
                }
                "--help" | "-h" => {
                    println!(
                        "usage: codex-transportd --socket <path> [--codex-home <path>] \
                         [--image-digest <digest>] [--build-proof-digest <digest>] \
                         [--socket-dir-uid <uid>] [--socket-dir-gid <gid>]"
                    );
                    println!(
                        "ownership: socket dir and volumes are deployment-prepared (uid 10001); \
                         this daemon verifies only and never chmod/chowns/deletes deployment resources"
                    );
                    return Err("help".to_string());
                }
                other => return Err(format!("unknown argument: {other}")),
            }
            i += 1;
        }

        let socket_path = socket_path.ok_or("--socket is required")?;
        let codex_home = match codex_home {
            Some(p) => p,
            None => env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex")))
                .ok_or("--codex-home is required (or set CODEX_HOME/HOME)")?,
        };

        Ok(Self {
            socket_path,
            codex_home,
            image_digest_override,
            build_proof_digest_override,
            socket_dir_expected_uid,
            socket_dir_expected_gid,
        })
    }
}
