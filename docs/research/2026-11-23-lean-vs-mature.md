# 会话台变重了吗：和成熟终端工具对比（2026-11-23，只读调研）

## 结论
- **整体不算臃肿。** 生产代码 31.7k 行（去掉内联测试；其中注释约 3.1k、空行约 2.3k，真正的代码约 26k 行），测试 25.8k 行。直接依赖 9 个，没有 tokio，二进制 5.4MB。拿同类比：atuin 新加的「读各家 AI 会话」那部分，每家的解析文件是我们的 2–6 倍大；shpool 光守护进程就约 12k 行，我们的 host 是 4.7k。
- **真正多余、而且会出 bug 的只有一处：** sources 读 SQLite 时调的是 `/usr/bin/sqlite3 -json` 命令行，但项目早已引入 rusqlite。为了绕开这条命令行管道，代码里堆了 4 类专门的补丁。这是排第一的建议。
- **能直接删的：** Qwen 适配器（已在待办，约 1.36k 行）；没有任何发布版用过的「无握手旧守护进程」兼容分支（约 180 行）；6 份重复的「带超时跑子进程」（约 120 行）。另外 `$` 余额面板约 2.5k 行，留不留由主人定。
- **重的是界面层那三个文件**（runtime 2434、app 2446、view 2197 行生产代码），也是改动最多的三个文件（git 里分别有 42、35、35 次提交）。问题在职责堆在一起，不在写法差：拆文件省不了几行，但能减少并行任务之间的合并冲突。新建对话认领那块逻辑已经修到第 3–4 轮，应该先把它收进一个独立文件。

## 一、重量在哪（实测）

| 模块 | 生产行数 | 说明 |
|---|---|---|
| ui | 9,490 | runtime.rs 2434（事件循环、10 个后台线程、认领逻辑）、app.rs 2446（`App` 一个 impl 有 127 个方法）、view.rs 2197（43 个直接画进 Buffer 的函数） |
| sources | 7,791 | 9 家适配器，平均每家 450–860 行；共用的 jsonl/window/transcript/moment/trash/account 约 2.3k 行 |
| host | 4,728 | daemon 1531、client 822、screen 694、scan 487、protocol 417 |
| quota | 2,413 | 其中余额渠道 balance.rs 812、keys.rs 520 |
| index | 2,317 | 缓存、FTS、监听、标注 |
| cli | 1,569 | doctor 593、update 362 |
| opener、core、notify、hotkey、ime、export | 共约 3.4k | 都不大 |

- **长函数不多：** 1573 个函数里只有 27 个超过 80 行，最长的是 view.rs 的 `top_bar` 161 行、`bottom_bar` 156 行、`settings` 150 行、`list` 147 行，都是画界面的代码。
- **适配器之间复制粘贴很少：** 同名函数（`tail_transcript`、`decode_dir`、`list_files`、`last_messages`、`parts`）逐个 diff 过，内容都不一样，是各家文件格式本来就不同。真正能共用的已经抽进了 `jsonl.rs`、`window.rs`、`transcript.rs`、`moment.rs`。每家 `impl Source` 的样板大约 60–100 行，就算做成一张表也省不到 200 行，反而要给每一家的 `parse_version` 加一，不划算。
- **厂商相关的逻辑漏到 sources 外面的只有一处：** `src/ui/runtime.rs:632` 的 `id_created` 对 9 家写了一个 match（调 `hermes::id_created`、`kimi::created_at` 等）。这个应该是 `Source` trait 上的一个方法。
- **加一家厂商要改 8 个 src 文件**（Antigravity 那次提交 74f0eb3）：vendor 表、dirs、protocol、mod 注册、account、runtime，加上 20 个界面快照（顶栏变了）。已经基本靠表驱动，可以接受。
- **状态文件：** config.json、annotations.json(+lock)、tabs.json、claims.json、running.json、interrupted.json、balance.json、update-check.json、索引库、daemon.log、socket/lock、回收站。每个都有 DECISIONS 里的理由（比如标注和可以重建的缓存分开存），不建议合并。
- **快捷键：** 37 个动作，全部集中在 `src/ui/keys.rs` 一张表里，这个做法和 gitui 的 `keys/key_list.rs` 一样，不用改。

## 二、成熟工具怎么做（看的是源码）

所有链接指向各仓库 main 分支，我看的时候各自的最新提交是：atuin 86573ef、yazi d5e4beb、gitui d7214ec、shpool 238b9fb。

1. **atuin：和我们的 sources 是同一件事。** 它新加的 `crates/atuin-common/src/harnesstools/` 也在读 Claude Code、Codex、pi、Hermes、OpenCode 等各家的会话，这一块总共 65.8k 行。
   - 每家的 `session.rs` 大小：opencode 5183、codex 3387、ccode 2679、pi 2377、hermes 1056。我们对应的是 861、699、346、258、539。**说明逐家解析的重量是这件事本身带来的，我们已经算省的。**
   - 结构：一个 `Harness` trait，每家实现一份，再用 `enum_dispatch` 汇成 `AnyHarness`（https://github.com/atuinsh/atuin/blob/main/crates/atuin-common/src/harnesstools/mod.rs）。trait 里的 `home_vars()` 就是我们的 `dir_vars`，`resume_plan()` 就是我们的 `resume_cmd`。形状和我们的 `Source` + `VENDORS` 一样，没有更好的套路可以抄。
   - **它读 SQLite 是在进程内（sqlx）**，比如 `codex/state_db.rs`、`hermes/store.rs`，不调命令行。
   - 旧的 shell 历史导入器（`crates/atuin-client/src/import/*.rs`）每个 150–330 行，trait 只有 3 个方法。它们只做一次性导入，我们的适配器还要做扫描、预览、整段、增量计数、恢复、回收站，不能直接比行数。
2. **gitui：每个弹窗一个文件。** `src/popups/*.rs` 每个弹窗一个文件，各自实现 `Component`（处理事件、列出命令）和 `DrawableComponent`（画），见 https://github.com/gitui-org/gitui/blob/master/src/components/mod.rs 。要动外部的事放进 `src/queue.rs` 的 `InternalEvent` 队列，交给 App 去做。总调度的 `app.rs` 只有 1229 行。**我们的 `App` + `Effect` 已经是同一个模式**（app.rs 不做 IO，Effect 交给 runtime 落地），`reader.rs`、`newdir.rs`、`balance.rs` 也已经是独立组件。差别只是 Settings、Menu、Board、Search、Prompt 的状态和按键处理还留在 app.rs，对应的绘制还留在 view.rs。
3. **yazi：每个命令一个文件。** `yazi-actor/src/mgr/` 下 70 多个文件，每个命令一个文件，实现 `Actor { type Form; const NAME; fn act(cx, form) }`（https://github.com/sxyazi/yazi/blob/main/yazi-actor/src/actor.rs）。界面按区域分目录（`yazi-fm/src/mgr/`、`help/`、`input/`……），状态放在 yazi-core。它的启发是**按功能分文件，改一个功能只碰一个文件**，而不是引入 Lua 插件那一整套机制，那对我们是过度设计。
4. **shpool：对应我们的托管守护进程。** 守护进程和 attach 约 12k 行，最大的三个文件 server.rs 1637、shell.rs 1327、events.rs 1179；另外还依赖 shpool_vt100、shpool-vterm、nix、crossbeam 等。它也有 `VersionHeader` 版本握手，协议单独放在 `shpool-protocol` 这个 crate 里。**我们的 host 4.7k 行做了同样的事，还多了标题改写和闲置收回，这部分重量是本来就该有的，不建议动。**
5. **体积参考**（各家最新 Release 的 macOS arm64 压缩包大小）：yazi 11MB、atuin 18MB、lazygit 6MB、shpool 1MB。我们是 5.4MB，而且这是没压缩的。

## 三、建议（按收益和风险排序）

### A. sources 读 SQLite 改用进程内的 rusqlite（收益高，风险中）
- **为什么：** DECISIONS 第 59 行当初就写了「届时若引入 rusqlite 再换掉命令行调用即可（接口不变）」。现在 rusqlite 已经因为 index 引进来了，但 `src/sources/sqlite.rs` 还在调 `/usr/bin/sqlite3 -readonly -json`，每次查询起一个子进程（本机实测约 3ms 一次），全项目有 27 处调用。下面这些都是专门绕开这条命令行管道写的补丁：
  1. `hermes.rs:50` 的 `TEXT_SQL`：命令行输出遇到 NUL 就截断，只好在 SQL 里按字节认 `\0json:` 前缀。
  2. `hermes.rs:390` 的 `time_sql`：`-json` 会把无穷大输出成裸的 `Inf`，导致整批结果解析失败。
  3. `opencode.rs:16,794,806`：所有文字列都先 `hex()` 传出来再解码，因为孤立代理项会让 `-json` 输出不合法的 UTF-8。
  4. `codex.rs:568` 的 `HISTORY_ITEMS = 3000`：「sqlite3 一次查询的输出有上限」。
  - 审计第 12 条（f9df627，「一条坏 JSON 让整家列表扫描失败」）也是这一类问题。待办里「OpenCode 孤立代理项换替换字符」那一条，改完之后大半也就不存在了。
- **怎么做：**
  - 第一步，只改 `sqlite.rs` 内部，保留 `query(db, sql) -> Vec<Row>` 这个签名（Row 还是 `Map<String, Value>`），适配器一行都不用动。
  - 打开方式：`SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI`，加 `busy_timeout(2s)`；遇到 `CANTOPEN` 就退到 `immutable=1` 的 URI，和现在的做法一样。现在 5 秒杀进程的硬超时，改成用 `progress_handler` 或另一个线程调 `interrupt()` 来实现。
  - 第二步，一家一家删掉上面这些补丁，改用 `ValueRef::Text`/`Blob` 直接拿字节、`from_utf8_lossy` 转文字。每家改一次 `parse_version` 加一。
- **能省：** sqlite.rs 约 150 行降到约 70 行，加上各家的补丁，合计大约少 150–250 行，还少了一整类「输出解析失败」的 bug。
- **风险：**
  - SQLite 改成在进程里跑，厂商库坏了不再隔在子进程里。不过 rusqlite 只读打开不会写库；坏库现在报的是 `NotADatabase`，原样返回错误就行。
  - 要确认 bundled 的 SQLite 版本（libsqlite3-sys 0.40）带 JSON 函数，OpenCode 和 Hermes 的查询里用到了 `json_each`。
  - 用 `tests/sources_*` 的 fixtures 对一遍改前改后的结果。
- **参考：** atuin 用进程内的 sqlx 读 Codex 状态库，见 https://github.com/atuinsh/atuin/blob/main/crates/atuin-common/src/harnesstools/codex/state_db.rs 。

### B. 照待办去掉 Qwen Code 和智谱 GLM（收益中，风险低，已排进待办）
- **能省：** qwen.rs 746 行，加上 Qwen 的测试和 fixtures 共 610 行，约 1.36k 行；GLM 在余额代码里约 30 行。
- **要守住的：** `Vendor::Qwen` 必须还能解码，待办里已经写了这一条。

### C. 新建对话的认领逻辑收进一个文件（降低 bug，不省行数，风险低）
- **为什么：** 这块已经修了好几轮：57e58e6 起步，e1536f3（审计修复），b2ff946（「认领要证据」），现在 agent/1123-claims-upgrade 又在修审计第 6、7、12、13、16 条。按 engineering.md 的规则，同一块为同一类问题修到第五次就该重写；而现在它散在两个文件里：
  - app.rs 2244–2446 行的 `Claims`/`Bound`/`Watch`；
  - runtime.rs 605–810 行的 `Hosted`、`hosted_alias`、`is_new_since`、`may_be`、`time_pairs`、`load_claims`/`save_claims`、`id_created`。
- **怎么做：**
  - 等在做的那条合并之后，把上面这些搬进一个不做 IO 的 `src/ui/claims.rs`，runtime 只调它的接口。
  - 同时把 `id_created` 的 match 改成 `Source` 上的一个默认方法 `created_at(&self, s) -> Option<SystemTime>`（默认返回 `None`），ui 里就不再出现具体厂商的名字。
  - `tests/ui_claims.rs` 已经存在，可以直接拿来固定行为。下次这块再出 bug，就只需要重写这一个文件。

### D. app.rs、view.rs、runtime.rs 按功能拆文件（减少冲突，不省行数，风险低）
- **为什么：** 这三个文件是改动最多的前三名，又都很大。git 里有 11 次提交跟合并冲突有关，冲突主要出在 DECISIONS 和 vendor.rs，ui 也有（43e0511 那次门检查因为冲突退回）。agentq 是按 worktree 并行做任务的，文件越集中，冲突越多。
- **怎么做（照 gitui 每个弹窗一个文件、yazi 每个功能一个文件的思路，但不引入新的 trait）：**
  - app.rs：Rust 允许同一个类型在多个文件里写 `impl App`。拆成 `ui/app/{search,board,menu,settings,prompt,mouse,claims}.rs`。按方法名分组看，`click` 121 行、`act` 97 行、`next_where` 61 行、`change_setting` 50 行，都能各自归到某个文件。
  - view.rs：按区域拆成 `ui/view/{top_bar,list,board,preview,bottom_bar,overlays}.rs`。
  - runtime.rs：把 10 个 `*_worker` 线程（1660–1990 行）挪进 `ui/workers.rs`，把 `Worker` 和 `Job`（2090–2360 行）挪进 `ui/jobs.rs`。
  - 纯搬家，行为不变，`sh .agentq/verify` 能通过就算完成。
- **不建议：** 引入 gitui 那种 `Component` trait 或 yazi 那种 Actor 宏去重写界面层。现在的 App + Effect 已经可测、也稳定，为了好看重写违反「稳定第一」。

### E. 6 份「带超时跑子进程」合成一份（收益小，风险低）
- **现在有 6 份几乎一样的 `spawn → try_wait 轮询 → 超时 kill`：** `sources/sqlite.rs:36`、`ui/runtime.rs:2360` 的 `run_child`/`wait_child`、`notify/deliver.rs:96`、`cli/summon.rs:104`、`opener/exec.rs:13`、`quota/exec.rs:9`。`host/client.rs:251` 是等守护进程起来，逻辑不一样，不算在内。
- **怎么做：** 按依赖方向，host、notify 只能依赖 core，所以只能放进 `core/exec.rs`（一个函数：程序、参数、可选的标准输入、超时，返回标准输出）。要在 AGENTS.md 的模块表里把 core 的说明从「纯数据」改成「纯数据 + 无依赖的小工具」。
- **能省：** 约 120 行（如果 A 先做，sqlite 那份会自己消失）。顺带让各处的行为一致：现在 `summon.rs` 是进程结束后才读标准输出，其他几份是边跑边读。

### F. 删掉「没有握手的旧守护进程」兼容分支（收益小，风险很低）
- **依据：** 版本握手（c4ffe48，2026-11-21 05:58 +0800）和订阅（84171e7）都早于第一个公开版 v0.1.0（2026-11-21T18:02Z，用 `gh release list -R dodo258/huitai` 核对过）。**没有任何发布出去的守护进程是不握手的**，这条分支只服务过发布前的开发版本。
- **删什么：**
  - `host/client.rs` 里的 `LEGACY`/`SocketKey`/`is_legacy`；
  - `probe` 里走 Ping+List 的回退；
  - `retire` 里发 SIGTERM 的分支；
  - `DaemonInfo.build` 为 `None` 的情况；
  - `tests/host_legacy_board.rs` 里测「不握手」的那一半。
- **保留：** 「会握手、但版本更旧、不认 Board」的降级路径（留给以后订阅格式变化时用），以及 `tests/host_old_client.rs`（对 v0.1.2 的兼容是真实需要的）。
- **能省：** 生产代码约 80 行，测试约 100 行。只动客户端这一侧，不涉及 protocol 的「只加不改」规则。

### G. （主人定）`$` 余额面板留不留
- **规模：** quota/balance.rs 812、keys.rs 520、ui/balance.rs 448，加 tests/quota_balance.rs 673 和规则文件，共约 2.5k 行。
- **为什么值得想一想：** 它要读 pi、Hermes 配置和环境变量里的 API 密钥，再发 HTTP 请求查余额。这是全项目唯一碰密钥的地方，刚因为「把网关密钥发给原厂」修过一次（8828cd9）。它也不属于「列出对话、接着聊」这个核心。
- **建议：** 主人平时如果不看 `$`，删掉它是除 Qwen 之外最大的一笔减重，也去掉了整个密钥相关的风险面；常用就留着。顶栏的订阅额度（quota/claude.rs、codex.rs）和余额是分开的，不受影响。

## 四、本来就该这么重、不建议动的
- **host 的 PTY 托管、终端状态机、重连：** 参照 shpool，已经算省的。
- **逐家解析：** 参照 atuin 的 harnesstools，各家格式差异是真实存在的。强行做一个通用的 JSONL 适配器，省不到 200 行，还要让 9 家的缓存全部重读。
- **index 的增量计数和 FTS、自建回收站、doctor、自更新：** 都有明确的用途和 DECISIONS 里的理由，也没有反复出问题的记录。
- **测试和生产代码约 0.8 : 1：** 对一个托管 PTY 的工具是正常比例。

## 五、能省多少

| 建议 | 生产代码 | 测试/fixtures | 降低 bug 的方面 |
|---|---|---|---|
| A rusqlite | −150 到 −250 | 部分补丁的测试可以简化 | 去掉一整类输出解析失败 |
| B 去 Qwen/GLM | −780 | −610 | — |
| C 认领收进一个文件 | ≈0 | 0 | 为可能的重写划清边界 |
| D 拆文件 | 0 | 0 | 减少并行任务的冲突 |
| E 子进程合一 | −120 | 合并测试 | 超时和读输出的行为一致 |
| F 去旧握手分支 | −80 | −100 | — |
| G 去余额（可选） | −1,800 | −700 | 去掉碰密钥的风险面 |

不算 G，大约少 1.1–1.2k 行生产代码、0.7k 行测试；算上 G，大约少 3k 行生产代码。

**建议的顺序：** B（已排好）→ 等进行中的认领任务合并后做 C → A → E、F → D 放在没有并行任务的空档做（纯搬家，最怕冲突）→ G 等主人一句话。
