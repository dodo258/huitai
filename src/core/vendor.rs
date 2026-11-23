use std::fmt;

use serde::{Deserialize, Serialize};

/// 厂商。界面里各占一个标签，互不混放。每家的静态信息在 [`VENDORS`] 一张表里，别处只查表。
///
/// `Qwen` 不再支持（不列出、不扫描），只为解码旧数据留着：旧守护进程的托管列表、`running.json`、旧标注、
/// 回收站里还会有 `"qwen"`，解码不了会让整份应答或文件失败。见 [`VendorInfo::listed`]。
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
    Antigravity,
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
    /// CLI 认的数据目录环境变量；CLI 不认任何改目录的变量（Antigravity）是空串，那家的数据永远在默认位置。
    pub dir_env: &'static str,
    /// 没设环境变量时数据目录在家目录下的哪里。
    pub default_dir: &'static str,
    /// `dir_env` 指的是 CLI 自己的家目录、数据目录在它下面的 `default_dir`（Gemini 的 `GEMINI_CLI_HOME`）；
    /// 别的家这个变量直接指数据目录。
    pub dir_env_is_home: bool,
    /// 会搬走数据目录的全部环境变量（含 `dir_env`），前面的盖过后面的（规则见 `VendorDirs::new`）。
    /// 一家有几个账号时，默认账号的命令前把它们全清掉；别的账号清掉排在它带的那个变量前面的。
    pub dir_vars: &'static [&'static str],
    /// 解析版本。改了这家的解析结果（`Session` 任何字段的取法、预览和正文的取法）就把它加一：
    /// index 记着每个会话文件、每条对话是哪个版本读的，版本比缓存新就整家重读，比缓存旧（同时开着旧版 `ai`）
    /// 就不覆盖新版写的结果。
    pub parse_version: u32,
    /// 新建对话时能不能由我们先定 id（`--session-id`）。不能的（Codex、Hermes、Kimi Code）托管在临时 id 下，
    /// 等它写出会话记录再认回来。
    pub presets_new_id: bool,
    /// 还支持不支持：不支持的（Qwen Code）不进 [`Vendor::ALL`]，不列出、不扫描、没有来源，
    /// 只为解码旧数据留着这一行（名字给旧版托管的标签页标题用，目录给 0.1.3 旧标注的目录名算法用）。
    pub listed: bool,
    /// 会话记录里认得出撞用量上限、读得出重置时间（`sources::limit`，有官方字样才算）。
    /// 认得出的家对话菜单里才有「撞额度后自动继续」。
    pub limit_notice: bool,
}

/// 每家一行，顺序就是界面标签的顺序，必须和 [`Vendor`] 的声明顺序一致（[`Vendor::info`] 按下标取）。
pub const VENDORS: [VendorInfo; 9] = [
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
        dir_vars: &["CLAUDE_CONFIG_DIR"],
        // 2：项目目录改取和会话文件所在目录对得上的 cwd（对话归错项目）。
        // 3：认出自动运行（`entrypoint` 为 `sdk-cli`）。
        // 4：头尾读到的 cwd 都对不上文件所在目录时，取对得上的上级目录（结尾在工作树里的长对话不再归 agentq 工作树）。
        parse_version: 4,
        presets_new_id: true,
        listed: true,
        limit_notice: true,
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
        dir_vars: &["CODEX_HOME"],
        // 2：认出自动运行（`source` 为 `exec`）。3：读 Codex 压缩归档的 rollout（`.jsonl.zst`）。
        // 4：正文库里的整段对话不再只取最后 3000 项，按读 rollout 同样的字节上限取。
        parse_version: 4,
        presets_new_id: false,
        listed: true,
        limit_notice: true,
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
        dir_vars: &["PI_CODING_AGENT_DIR"],
        parse_version: 1,
        presets_new_id: true,
        listed: true,
        limit_notice: false,
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
        dir_vars: &["HERMES_HOME"],
        // 3：库改在进程内读（rusqlite）：字里有坏 UTF-8、中间带 NUL 时取全、只坏那个字，不再让整家列表失败。
        parse_version: 3,
        presets_new_id: false,
        listed: true,
        limit_notice: false,
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
        dir_vars: &["KIMI_CODE_HOME"],
        // 2：撤销过的轮次不显示、不计数；头尾都没读到用户发言时从头找一遍。
        parse_version: 3,
        presets_new_id: false,
        listed: true,
        limit_notice: false,
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
        dir_vars: &["GEMINI_CLI_HOME"],
        // 2：标题取头尾之间最后一条 `$set.summary`（长对话改过的标题不再退回首句）。
        parse_version: 2,
        presets_new_id: true,
        listed: true,
        limit_notice: false,
    },
    VendorInfo {
        vendor: Vendor::OpenCode,
        key: "opencode",
        mark: '⌬',
        name: "OpenCode",
        accent_dark: 0x5fb3a1,
        accent_light: 0x2b7a69,
        // OpenCode 认的是库文件（`OPENCODE_DB`），不是目录：这里存的就是库的路径。
        // 相对路径和 `XDG_DATA_HOME` 的规则见 `VendorDirs::new`。
        dir_env: "OPENCODE_DB",
        default_dir: ".local/share/opencode/opencode.db",
        dir_env_is_home: false,
        dir_vars: &["OPENCODE_DB", super::dirs::XDG_DATA_HOME],
        // 2：预览按有字的消息往前取足（连续只调工具的几十步后预览不再是空的）。
        // 3：1.x 撤销（`session.revert`）之后的不显示、不计数；坏字符只坏那个字；2.x 标题跳过坏了的第一句。
        // 4：整段里孤立代理项的 JSON 转义只换那个字。
        // 5：库改在进程内读（rusqlite），字不再经 hex() 传出（坏字照样只坏那个字）。
        parse_version: 5,
        // 2.x 的 `-s <id>` 遇到不存在的 id 会用它新建，1.x 直接报错退出：两版都能用的只有临时 id 托管再认回。
        presets_new_id: false,
        listed: true,
        limit_notice: false,
    },
    VendorInfo {
        vendor: Vendor::Qwen,
        key: "qwen",
        mark: 'Q',
        name: "Qwen Code",
        accent_dark: 0xc57bdc,
        accent_light: 0x8e44a8,
        // 不再支持（2026-11-23），见 `listed`。目录照 0.1.3 只看 `QWEN_HOME`：`VendorDirs::legacy` 要按旧版算旧标注的目录名。
        dir_env: "QWEN_HOME",
        default_dir: ".qwen",
        dir_env_is_home: false,
        dir_vars: &["QWEN_HOME"],
        parse_version: 2,
        presets_new_id: true,
        listed: false,
        limit_notice: false,
    },
    VendorInfo {
        vendor: Vendor::Antigravity,
        key: "antigravity",
        mark: 'A',
        name: "Antigravity",
        accent_dark: 0x8fbf4f,
        accent_light: 0x557a1f,
        // 官方文档没写改数据目录的变量（https://antigravity.google/docs/cli/reference/），只认默认位置。
        // `agy` 写 `~/.gemini/antigravity-cli`；Antigravity App 的 `~/.gemini/antigravity` 自动认成第二个账号，见 `sources::accounts`。
        dir_env: "",
        default_dir: ".gemini/antigravity-cli",
        dir_env_is_home: false,
        dir_vars: &[],
        // 2：正文（预览取最后若干条）从最新块往前读，最多往回读 8MB。
        // 3：库改在进程内读（rusqlite），没走 hex() 的字有坏 UTF-8 时只坏那个字，不再让整句查询失败。
        parse_version: 3,
        // `agy` 没有让我们先定 id 的选项。
        presets_new_id: false,
        listed: true,
        limit_notice: false,
    },
];

/// 还支持的有几家。
const LISTED: usize = {
    let mut n = 0;
    let mut i = 0;
    while i < VENDORS.len() {
        if VENDORS[i].listed {
            n += 1;
        }
        i += 1;
    }
    n
};

impl Vendor {
    /// 按家分开放的数组的长度：每个值一格，含不再支持的（它那一格永远空着，旧数据里的值落进来也不越界）。
    pub const SLOTS: usize = VENDORS.len();

    /// 全部值（含不再支持的），按声明顺序。只给要和旧版算法一致的地方用，列出、扫描一律用 [`Vendor::ALL`]。
    pub const EVERY: [Vendor; Vendor::SLOTS] = {
        let mut all = [Vendor::Claude; Vendor::SLOTS];
        let mut i = 0;
        while i < Vendor::SLOTS {
            all[i] = VENDORS[i].vendor;
            i += 1;
        }
        all
    };

    /// 还支持的各家，界面标签顺序。
    pub const ALL: [Vendor; LISTED] = {
        let mut all = [Vendor::Claude; LISTED];
        let (mut i, mut n) = (0, 0);
        while i < Vendor::SLOTS {
            if VENDORS[i].listed {
                all[n] = VENDORS[i].vendor;
                n += 1;
            }
            i += 1;
        }
        all
    };

    /// 还支持不支持，见 [`VendorInfo::listed`]。
    pub fn listed(self) -> bool {
        self.info().listed
    }

    /// 这家在 [`VENDORS`] 里的位置（按家分开放的数组用它当下标，数组长 [`Vendor::SLOTS`]）。
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
            assert_eq!(Vendor::EVERY[i], info.vendor);
        }
    }

    /// 只列还支持的八家；Qwen Code 不列出，但值还在、还能解码（旧数据里有它）。
    #[test]
    fn qwen_is_retired_but_still_decodes() {
        assert_eq!(Vendor::ALL.len(), 8);
        assert!(!Vendor::ALL.contains(&Vendor::Qwen));
        assert!(!Vendor::Qwen.listed());
        assert!(Vendor::ALL.iter().all(|v| v.listed()));
        assert_eq!(
            serde_json::from_str::<Vendor>("\"qwen\"").unwrap(),
            Vendor::Qwen
        );
        assert_eq!(serde_json::to_string(&Vendor::Qwen).unwrap(), "\"qwen\"");
    }

    /// 清目录变量时按 `dir_vars` 找 `dir_env` 的位置：每家都要列上自己的 `dir_env`。
    #[test]
    fn dir_vars_include_dir_env() {
        for info in VENDORS.iter().filter(|i| !i.dir_env.is_empty()) {
            assert!(info.dir_vars.contains(&info.dir_env), "{}", info.name);
        }
    }

    #[test]
    fn marks_names_keys_and_colors_are_distinct() {
        assert_eq!(
            Vendor::ALL.map(Vendor::mark),
            ['✻', '◎', 'π', '☤', 'K', '✦', '⌬', 'A']
        );
        assert_eq!(Vendor::Pi.to_string(), "pi");
        for (i, a) in VENDORS.iter().enumerate() {
            for b in &VENDORS[i + 1..] {
                assert_ne!(a.mark, b.mark);
                assert_ne!(a.key, b.key);
                if !a.dir_env.is_empty() {
                    assert_ne!(a.dir_env, b.dir_env);
                }
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
        assert!(serde_json::from_str::<Vendor>("\"qwen-code\"").is_err());
    }
}
