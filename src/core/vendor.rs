use std::fmt;

use serde::{Deserialize, Serialize};

/// 厂商。界面里各占一个标签，互不混放。每家的静态信息在 [`VENDORS`] 一张表里，别处只查表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vendor {
    Claude,
    Codex,
    Pi,
    Hermes,
    Kimi,
    Gemini,
    OpenCode,
    Qwen,
}

/// 一家的静态信息。
#[derive(Debug)]
pub struct VendorInfo {
    pub vendor: Vendor,
    /// 存储用的小写名：序列化、列表缓存、回收站目录都用它，定了不改。
    pub key: &'static str,
    /// 标记字符，用在标签、列表和标签页标题前缀。
    pub mark: char,
    /// 显示名。
    pub name: &'static str,
    /// 强调色（深色终端），`0xRRGGBB`。
    pub accent_dark: u32,
    /// 强调色（浅色终端），压暗一档保证白底上的对比度。
    pub accent_light: u32,
    /// CLI 认的数据目录环境变量。
    pub dir_env: &'static str,
    /// 没设环境变量时数据目录在家目录下的哪里。
    pub default_dir: &'static str,
    /// `dir_env` 指的是 CLI 自己的家目录、数据目录在它下面的 `default_dir`（Gemini 的 `GEMINI_CLI_HOME`）；
    /// 别的家这个变量直接指数据目录。
    pub dir_env_is_home: bool,
    /// 解析版本。改了这家的解析结果（`Session` 任何字段的取法、预览和正文的取法）就把它加一：
    /// index 记着每个会话文件、每条对话是哪个版本读的，版本比缓存新就整家重读，比缓存旧（同时开着旧版 `ai`）
    /// 就不覆盖新版写的结果。
    pub parse_version: u32,
    /// 新建对话时能不能由我们先定 id（`--session-id`）。不能的（Codex、Hermes、Kimi Code）托管在临时 id 下，
    /// 等它写出会话记录再认回来。
    pub presets_new_id: bool,
}

/// 每家一行，顺序就是界面标签的顺序，必须和 [`Vendor`] 的声明顺序一致（[`Vendor::info`] 按下标取）。
pub const VENDORS: [VendorInfo; 8] = [
    VendorInfo {
        vendor: Vendor::Claude,
        key: "claude",
        mark: '✻',
        name: "Claude",
        accent_dark: 0xd97757,
        accent_light: 0xc0603f,
        dir_env: "CLAUDE_CONFIG_DIR",
        default_dir: ".claude",
        dir_env_is_home: false,
        // 2：项目目录改取和会话文件所在目录对得上的 cwd（对话归错项目）。
        // 3：认出自动运行（`entrypoint` 为 `sdk-cli`）。
        // 4：头尾读到的 cwd 都对不上文件所在目录时，取对得上的上级目录（结尾在工作树里的长对话不再归 agentq 工作树）。
        parse_version: 4,
        presets_new_id: true,
    },
    VendorInfo {
        vendor: Vendor::Codex,
        key: "codex",
        mark: '◎',
        name: "Codex",
        accent_dark: 0x6e95e8,
        accent_light: 0x3c6dd0,
        dir_env: "CODEX_HOME",
        default_dir: ".codex",
        dir_env_is_home: false,
        // 2：认出自动运行（`source` 为 `exec`）。
        parse_version: 2,
        presets_new_id: false,
    },
    VendorInfo {
        vendor: Vendor::Pi,
        key: "pi",
        mark: 'π',
        name: "pi",
        accent_dark: 0xa37fe6,
        accent_light: 0x7b53c8,
        dir_env: "PI_CODING_AGENT_DIR",
        default_dir: ".pi/agent",
        dir_env_is_home: false,
        parse_version: 1,
        presets_new_id: true,
    },
    VendorInfo {
        vendor: Vendor::Hermes,
        key: "hermes",
        mark: '☤',
        name: "Hermes",
        accent_dark: 0xbfae3a,
        accent_light: 0x766c12,
        dir_env: "HERMES_HOME",
        default_dir: ".hermes",
        dir_env_is_home: false,
        parse_version: 1,
        presets_new_id: false,
    },
    VendorInfo {
        vendor: Vendor::Kimi,
        key: "kimi",
        // 方案里的 `◐` 和在跑的转圈撞，用 `K`。
        mark: 'K',
        name: "Kimi Code",
        accent_dark: 0xe06c9f,
        accent_light: 0xb03f72,
        dir_env: "KIMI_CODE_HOME",
        default_dir: ".kimi-code",
        dir_env_is_home: false,
        parse_version: 1,
        presets_new_id: false,
    },
    VendorInfo {
        vendor: Vendor::Gemini,
        key: "gemini",
        mark: '✦',
        name: "Gemini",
        accent_dark: 0x4f9fe0,
        accent_light: 0x1f78b8,
        // `GEMINI_CLI_HOME` 换的是家目录（上游 `utils/paths.ts` 的 `homedir()`），会话在它下面的 `.gemini/tmp`。
        // 数据目录取到 `tmp`：`~/.gemini` 别的工具也会建（Antigravity 的 `~/.gemini/antigravity`），用过 Gemini CLI 才有 `tmp`。
        dir_env: "GEMINI_CLI_HOME",
        default_dir: ".gemini/tmp",
        dir_env_is_home: true,
        parse_version: 1,
        presets_new_id: true,
    },
    VendorInfo {
        vendor: Vendor::OpenCode,
        key: "opencode",
        mark: '⌬',
        name: "OpenCode",
        accent_dark: 0x5fb3a1,
        accent_light: 0x2b7a69,
        // OpenCode 认的是库文件（`OPENCODE_DB`），不是目录：这里存的就是库的路径。
        dir_env: "OPENCODE_DB",
        default_dir: ".local/share/opencode/opencode.db",
        dir_env_is_home: false,
        parse_version: 1,
        // 2.x 的 `-s <id>` 遇到不存在的 id 会用它新建，1.x 直接报错退出：两版都能用的只有临时 id 托管再认回。
        presets_new_id: false,
    },
    VendorInfo {
        vendor: Vendor::Qwen,
        key: "qwen",
        mark: 'Q',
        name: "Qwen Code",
        accent_dark: 0xc57bdc,
        accent_light: 0x8e44a8,
        dir_env: "QWEN_HOME",
        default_dir: ".qwen",
        dir_env_is_home: false,
        parse_version: 1,
        presets_new_id: true,
    },
];

impl Vendor {
    /// 一共几家。
    pub const COUNT: usize = VENDORS.len();

    /// 界面标签顺序。
    pub const ALL: [Vendor; Vendor::COUNT] = {
        let mut all = [Vendor::Claude; Vendor::COUNT];
        let mut i = 0;
        while i < Vendor::COUNT {
            all[i] = VENDORS[i].vendor;
            i += 1;
        }
        all
    };

    /// 这家在 [`Vendor::ALL`] 里的位置（按家分开放的数组用它当下标）。
    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn info(self) -> &'static VendorInfo {
        &VENDORS[self.index()]
    }

    /// 厂商标记字符，用在标签和标签页标题前缀。
    pub fn mark(self) -> char {
        self.info().mark
    }

    /// 显示名。
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// 存储用的小写名，见 [`VendorInfo::key`]。
    pub fn key(self) -> &'static str {
        self.info().key
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
    fn table_follows_declaration_order() {
        for (i, info) in VENDORS.iter().enumerate() {
            assert_eq!(info.vendor.index(), i, "{} 的行放错了位置", info.name);
            assert_eq!(Vendor::ALL[i], info.vendor);
        }
    }

    #[test]
    fn marks_names_keys_and_colors_are_distinct() {
        let all = Vendor::ALL;
        assert_eq!(
            all.map(Vendor::mark),
            ['✻', '◎', 'π', '☤', 'K', '✦', '⌬', 'Q']
        );
        assert_eq!(Vendor::Pi.to_string(), "pi");
        for (i, a) in VENDORS.iter().enumerate() {
            for b in &VENDORS[i + 1..] {
                assert_ne!(a.mark, b.mark);
                assert_ne!(a.key, b.key);
                assert_ne!(a.dir_env, b.dir_env);
                assert_ne!(a.accent_dark, b.accent_dark);
                assert_ne!(a.accent_light, b.accent_light);
            }
        }
    }

    #[test]
    fn serializes_as_key() {
        for v in Vendor::ALL {
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                format!("\"{}\"", v.key())
            );
        }
        assert_eq!(
            serde_json::from_str::<Vendor>("\"hermes\"").unwrap(),
            Vendor::Hermes
        );
        assert_eq!(
            serde_json::from_str::<Vendor>("\"gemini\"").unwrap(),
            Vendor::Gemini
        );
        assert_eq!(
            serde_json::from_str::<Vendor>("\"opencode\"").unwrap(),
            Vendor::OpenCode
        );
        assert_eq!(
            serde_json::from_str::<Vendor>("\"qwen\"").unwrap(),
            Vendor::Qwen
        );
        assert!(serde_json::from_str::<Vendor>("\"qwen-code\"").is_err());
    }
}
