use std::fmt;

use serde::{Deserialize, Serialize};

/// 三家厂商。界面里各占一个标签，互不混放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Claude,
    Codex,
    Pi,
}

impl Vendor {
    /// 界面标签顺序。
    pub const ALL: [Vendor; 3] = [Vendor::Claude, Vendor::Codex, Vendor::Pi];

    /// 厂商标记字符，用在标签和标签页标题前缀。
    pub fn mark(self) -> char {
        match self {
            Vendor::Claude => '✻',
            Vendor::Codex => '◎',
            Vendor::Pi => 'π',
        }
    }

    /// 显示名。
    pub fn name(self) -> &'static str {
        match self {
            Vendor::Claude => "Claude",
            Vendor::Codex => "Codex",
            Vendor::Pi => "pi",
        }
    }
}

impl fmt::Display for Vendor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_and_names_are_distinct() {
        assert_eq!(Vendor::ALL.map(Vendor::mark), ['✻', '◎', 'π']);
        assert_eq!(Vendor::Pi.to_string(), "pi");
    }

    #[test]
    fn serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Vendor::Codex).unwrap(), "\"codex\"");
        assert_eq!(
            serde_json::from_str::<Vendor>("\"claude\"").unwrap(),
            Vendor::Claude
        );
        assert!(serde_json::from_str::<Vendor>("\"gemini\"").is_err());
    }
}
