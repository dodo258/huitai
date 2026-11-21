# 参与开发

会话台是一个 Rust crate，产出一个二进制 `ai`，界面用 ratatui。只支持 macOS。

## 跑起来

```sh
cargo run              # 会话台
cargo run -- doctor    # 自检
```

提交前跑全部检查（和 CI 一样）：

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

托管层（守护进程）另有手动跑的压力测试和长跑，见 [tests/stress/README.md](tests/stress/README.md)。

## 模块

依赖只能向下：`cli → ui → (index, quota, opener, host 客户端, ime) → sources → core`；`host`、`notify`、`hotkey`、`ime` 只依赖 `core`。模块之间只走下表的接口，每块都能单独测试、单独重写。

| 路径 | 只负责 | 对外接口 |
|---|---|---|
| `src/core/` | 共享类型和配置 | 纯数据 |
| `src/sources/` | 三家适配器：扫描、元数据、预览、恢复/新建命令、回收站 | `trait Source` |
| `src/index/` | 缓存、增量扫描（FSEvents）、全文搜索（SQLite FTS5）、改名/置顶/归档 | `Index` |
| `src/host/` | 守护进程：PTY 托管、原样转发、重连恢复画面、标题改写、闲置收回、socket 协议 | `Hello/Spawn/Attach/Detach/Kill/List/Events` |
| `src/opener/` | 识别终端、开新标签页、聚焦已开的标签页 | `trait Opener` |
| `src/quota/` | 多渠道额度读取与降级 | `quota(vendor) -> QuotaReading` |
| `src/notify/` | 状态变化 → 提醒 | `notify(event)` |
| `src/hotkey/` | 全局呼出键 | `register(key, cb)` |
| `src/ime/` | 会话台在前台切英文键盘布局、离开切回 | `trait InputSources` |
| `src/ui/` | 界面、键鼠、主题（颜色集中在 `src/ui/theme.rs`） | 只调上面模块 |
| `src/cli/` | 子命令：`ai`、`ai doctor`、`ai update`、`ai attach`、`ai daemon` | — |

## 几条硬规矩

- 只读三家的会话文件，不改它们的配置；唯一会移动厂商文件的是「删除进回收站」。
- 托管层对 PTY 字节原样转发，只改写窗口标题序列（OSC 0/1/2）。改别的会破坏粘贴、换行和键盘协议。
- socket 协议（`src/host/protocol.rs`）的字段只加不改，新字段带 `#[serde(default)]`。
- 改了 `src/sources` 的解析结果，就把那家的 `parse_version` 加一，否则旧缓存不会重新解析。
- 测试数据一律手写的假样例（`tests/fixtures/`），不放任何真实对话。
- 会弹窗、开标签页、抢全局按键的真机行为用假实现测，不在自动测试里真的做。

## 发版

```sh
scripts/build-release.sh dist/   # 两种芯片的 ai、.sha256、install.sh、SHA256SUMS
```

打 `v<版本>` 标签，把 `dist/` 里的文件传到 GitHub Release。

---

# Contributing (English)

huitai is a single Rust crate producing one binary, `ai`, with a ratatui interface. macOS only. Run it with `cargo run`, check everything with `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`. The module table above lists what each module owns; dependencies only point downwards. Ground rules: never modify the three tools' configuration, pass PTY bytes through untouched except window titles, only add protocol fields, bump `parse_version` when parsing changes, and never commit real conversations as test data.
