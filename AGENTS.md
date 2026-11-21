# 项目地图（保持精简，只写稳定信息）

## 项目是什么
会话台（huitai）：装在终端里的命令行工具（不是终端模拟器），命令 `ai`。按厂商（Claude Code / Codex / pi）分开、按项目分组列出本机全部 AI 对话，回车在 Ghostty / Kaku 新标签页里接着聊；对话由会话台的透传托管守护进程保活，关窗口不停。只支持 macOS。完整方案：`docs/specs/huitai-v1.md`。

## 技术栈与目录
- Rust（单个 crate，单个二进制 `ai`），界面 ratatui。索引缓存和全文搜索用 SQLite FTS5（rusqlite bundled），监听目录用 notify（FSEvents）。
- `src/main.rs` — 入口；`src/lib.rs` — 声明下面所有模块。
- `src/<模块>/` — 各模块，见下表。
- `tests/fixtures/` — 三家会话文件的脱敏样例；`tests/` — 集成测试。
- `docs/specs/` — 方案；`docs/TEST-MATRIX.md` — 手工测试矩阵。

## 模块
依赖只能向下：`cli → ui → (index, quota, opener, host 客户端, ime) → sources → core`；`host`、`notify`、`hotkey`、`ime` 只依赖 `core`。模块之间只走下列接口。

| 路径 | 只负责 | 对外接口 |
|---|---|---|
| `src/core/` | 共享类型和配置：Vendor、Session、SessionId、SessionState、QuotaReading、Config | 纯数据 |
| `src/sources/` | 三家适配器：扫描、元数据、预览、恢复/新建命令、回收站 | `trait Source` |
| `src/index/` | 缓存、增量扫描（FSEvents）、全文搜索、会话台自己的标注（改名/置顶/归档） | `Index` |
| `src/host/` | 守护进程：PTY 托管、原样转发、终端状态机重连恢复、标题改写、通知序列识别、防休眠、闲置收回、socket 协议和版本握手 | `Hello/Spawn/Attach/Detach/Kill/List/Events` |
| `src/opener/` | 识别终端、开新标签页、聚焦已开标签页 | `trait Opener` |
| `src/quota/` | 多渠道额度读取与降级，规则文件 | `quota(vendor) -> QuotaReading` |
| `src/notify/` | 状态变化 → 通知 | `notify(event)` |
| `src/hotkey/` | 全局热键（Carbon RegisterEventHotKey） | `register(key, cb)` |
| `src/ime/` | 输入源：会话台在前台切英文键盘布局、离开切回（Carbon TIS，只在主线程调） | `trait InputSources`、`Switcher` |
| `src/ui/` | ratatui 界面、键鼠、主题 | 只调上面模块 |
| `src/cli/` | 子命令：`ai`、`ai doctor`、`ai attach <id>`、`ai daemon` | — |

## 常用命令
- 检查（与 `.agentq/verify` 一致）：`cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
- 构建：`cargo build --release`（产物 `target/release/ai`）
- 运行：`cargo run -- `（会话台）、`cargo run -- doctor`
- 官网（`site/`）：改了样式、脚本或截图先跑 `scripts/stamp-site.sh` 给引用盖版本号再提交、部署（官网域名浏览器缓存 4 小时，不盖章访问过的人会拿到旧文件）。
- 托管层稳定性：`cargo test --test stress stability`（随 `cargo test` 跑）；手动跑的压力测试 `tests/stress/run.sh`（约 32 分钟）、长跑 `tests/stress/run.sh soak`（默认 6 小时，`SOAK_MINUTES` 改时长），说明见 `tests/stress/README.md`。

## 约定
- 只读厂商数据；唯一会移动厂商文件的是「删除进回收站」。绝不修改 `~/.claude/settings.json`、`~/.codex/config.toml`、`~/.pi/agent/*`。
- 托管层对 PTY 字节原样转发，只允许改写窗口标题序列（OSC 0/1/2，加厂商标记和项目名前缀）；其余任何改写都会破坏粘贴、换行、键盘协议。
  - 例外：`ai attach` 客户端为了知道标签页在不在前台（在前台就不提醒），CLI 没开焦点上报时自己在真终端开 `CSI ?1004h`（断开时关），并把终端因此发来的 `ESC [ I` / `ESC [ O` 从输入里去掉、改用协议的 `F` 帧告诉守护进程（守护进程广播成 `HostEvent::Focus`）；CLI 自己开了 ?1004 时不开不关，序列原样转给它。这是撤掉我们自己要来的序列，不改 CLI 收发的任何字节。
- 守护进程必须从终端里拉起（setsid 脱离），不能用 launchd：否则 CLI 的辅助功能/屏幕录制/浏览器权限归属会变。
- CLI 子进程用登录 shell（`$SHELL -lc`）在项目目录启动，环境变量继承第一次打开它的终端标签页。
- 测试和截图不得把本机真实会话内容写进仓库；fixtures 一律手写脱敏样例。
- 自动跑的任务不做会在屏幕上弹窗或开窗口的真机测试（开 Ghostty/Kaku 标签页、系统通知、全局热键、登录项、AppleScript 自动化）：用单测和假实现覆盖，真机项写进 `docs/TEST-MATRIX.md`，由人在真机上测。
- 界面颜色、快捷键以方案「界面」一节为准，颜色集中在 `src/ui/theme.rs`。
- 改了 sources 的解析结果（`Session` 任何字段、预览或正文的取法）就把该家解析版本加一（`src/sources/mod.rs` 的 `parse_version`），否则旧缓存里没变的文件永远是旧结果。
- socket 协议（`src/host/protocol.rs`）和 `running.json` 里的字段只加不改，新字段带 `#[serde(default)]`；协议有不兼容的改动才把 `PROTOCOL` 加一，新旧客户端和守护进程怎么相处见 DECISIONS.md「版本握手」。
- 索引缓存只加列时不改 `SCHEMA_VERSION`，在 `src/index/store.rs` 的 `ADDED_COLUMNS` 补列：改版本号会让同时开着的旧版把整个库删了重建。

## 进度与决定
- 当前进度见 PROGRESS.md，历史决定见 DECISIONS.md。
