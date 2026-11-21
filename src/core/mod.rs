//! 共享类型和配置：纯数据，除配置文件读写外不做 IO。

mod config;
mod dirs;
mod quota;
pub mod release;
mod session;
mod vendor;

pub use config::Config;
pub use dirs::VendorDirs;
pub use quota::{QuotaReading, QuotaValue, QuotaWindow};
pub use session::{REDRAW_GRACE, Session, SessionId, SessionState};
pub use vendor::Vendor;
