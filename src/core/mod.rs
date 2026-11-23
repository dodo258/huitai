//! 共享类型和配置：纯数据，除配置文件读写外不做 IO；外加不依赖别的模块的小工具（[`exec`] 带超时跑子进程，[`http`] 用 curl 发 GET）。

mod account;
mod config;
mod dirs;
pub mod exec;
pub mod http;
mod quota;
pub mod release;
mod session;
mod vendor;

pub use account::{Account, AccountSetting};
pub use config::Config;
pub use dirs::{EnvAssign, VendorDirs};
pub use quota::{QuotaReading, QuotaValue, QuotaWindow};
pub use session::{QUIET_AFTER, REDRAW_GRACE, Session, SessionId, SessionState};
pub use vendor::{VENDORS, Vendor, VendorInfo};
