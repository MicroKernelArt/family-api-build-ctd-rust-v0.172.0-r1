//! codex-transportd 库入口（bin 复用同一模块集，便于集成测试）。

extern crate self as codex_transportd;

pub mod bootstrap;
pub mod build_info;
pub mod capture;
/// capture 的私有兄弟模块：进程内共享字节预算（F2/R2-F3；仅 capture /
/// capture_writer 消费，AM4 拆分）。
mod capture_budget;
/// capture 的私有兄弟模块：fd 锚定 writer（仅 capture 消费，AM4 拆分）。
mod capture_writer;
pub mod config;
pub mod contract;
pub mod limits;
pub mod observability;
pub mod ops_fixture;
pub mod redaction;
pub mod server;
pub mod transport;
