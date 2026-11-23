use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{EnvAssign, Vendor, VendorDirs};

/// 一家的一个账号：一个数据目录（`CLAUDE_CONFIG_DIR`、`CODEX_HOME`、Hermes profile 各算一个）。
/// 同一家几个账号的对话混在一个列表里按项目排，每条记着自己是哪个账号的（[`crate::core::Session::account`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub vendor: Vendor,
    /// 存储用的短名，见 [`Account::key_for`]。
    pub key: String,
    /// 显示名：设置里起的名字、Hermes profile 名；没起名的叫「默认」或目录名。
    pub name: String,
    /// 数据目录，和 [`VendorDirs::dir`] 同一种写法（绝对路径、规范化过）。
    pub dir: PathBuf,
}

impl Account {
    pub fn new(vendor: Vendor, name: impl Into<String>, dir: PathBuf, home: &Path) -> Self {
        Self {
            vendor,
            key: Self::key_for(vendor, &dir, home),
            name: name.into(),
            dir,
        }
    }

    /// 账号短名：数据目录在家目录下默认位置的是空串（老用户的对话 id、缓存、改名置顶都不变）；
    /// 别的目录是目录路径的 8 位十六进制摘要，同一个目录永远一样，换了环境变量启动也认得出。
    pub fn key_for(vendor: Vendor, dir: &Path, home: &Path) -> String {
        if dir == VendorDirs::defaults(home).dir(vendor) {
            return String::new();
        }
        // FNV-1a：std 的哈希不保证跨版本稳定，这里要跨版本不变。
        let mut h: u32 = 0x811c_9dc5;
        let bytes = vendor.key().as_bytes().iter().chain(&[0]);
        for b in bytes.chain(dir.as_os_str().as_encoded_bytes()) {
            h ^= u32::from(*b);
            h = h.wrapping_mul(0x0100_0193);
        }
        format!("{h:08x}")
    }

    /// 在家目录下默认位置的那个账号。
    pub fn is_default(&self) -> bool {
        self.key.is_empty()
    }

    /// 启动这个账号的 CLI 要怎么设这家改目录的变量（默认位置是空的），见 [`VendorDirs::env_for`]。
    pub fn env(&self, home: &Path) -> Vec<EnvAssign> {
        VendorDirs::env_for(self.vendor, &self.dir, home)
    }
}

/// 设置里手动加的账号：哪家、叫什么、数据目录（`~` 开头指家目录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSetting {
    pub vendor: Vendor,
    pub name: String,
    pub dir: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dir_has_empty_key_others_are_stable() {
        let h = Path::new("/h");
        let a = Account::new(Vendor::Claude, "默认", PathBuf::from("/h/.claude"), h);
        assert!(a.is_default());
        assert_eq!(a.env(h), []);
        let b = Account::new(
            Vendor::Claude,
            "工作号",
            PathBuf::from("/h/.claude-work"),
            h,
        );
        assert_eq!(b.key.len(), 8);
        assert_eq!(
            b.key,
            Account::key_for(Vendor::Claude, Path::new("/h/.claude-work"), h)
        );
        assert_ne!(
            b.key,
            Account::key_for(Vendor::Codex, Path::new("/h/.claude-work"), h)
        );
        assert_eq!(
            b.env(h),
            [("CLAUDE_CONFIG_DIR", Some(PathBuf::from("/h/.claude-work")))]
        );
        // 写死的值：换版本也不能变，变了老用户的改名置顶就对不上了。
        assert_eq!(b.key, "d8a3ab19");
    }
}
