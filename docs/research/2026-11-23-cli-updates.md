# 各家 CLI 安装方式、查新版、升级（调研 2026-11-23）

方法：读各家官方文档和源码，本机只读核对（未执行任何安装或升级）。汇总表见 docs/specs/cli-updates.md。

## 来源
- Claude Code：https://code.claude.com/docs/en/setup （`DISABLE_AUTOUPDATER`、`DISABLE_UPDATES`、`autoUpdatesChannel`、`CLAUDE_CODE_PACKAGE_MANAGER_AUTO_UPDATE`）
- Codex：https://github.com/openai/codex README；源码 codex-rs/tui/src/update_action.rs、updates.rs、codex-rs/install-context/src/lib.rs（每 20 小时检查，`check_for_update_on_startup`）
- pi：https://pi.dev ；包内 docs/cli.md、docs/environment-variables.md、dist/utils/version-check.js（`detectInstallMethod`、`PI_SKIP_VERSION_CHECK`、`PI_SELF_UPDATE_COMMAND`）
- Hermes：https://hermes-agent.nousresearch.com/docs/getting-started/installation 、/updating（`hermes update` 快照回滚、重启 gateway）
- Kimi Code：https://moonshotai.github.io/kimi-code/en/guides/getting-started ；dist/main.mjs 的 detectInstallSource、canAutoInstall（`KIMI_CODE_NO_AUTO_UPDATE`）
- Gemini CLI：https://github.com/google-gemini/gemini-cli ；packages/cli/src/utils/installationInfo.ts、handleAutoUpdate.ts（`general.enableAutoUpdate` 默认开）
- OpenCode：https://opencode.ai/docs/ 、/docs/config/（v2 `update`: disable/notify/auto）；v1.19.6 packages/opencode/src/installation/index.ts、cli/cmd/upgrade.ts（`--major` 才跨大版本）
- Antigravity：https://antigravity.google/docs/getting-started?tab=cli 、/docs/cli/troubleshooting（`AGY_CLI_DISABLE_AUTO_UPDATE`、update.lock）

## 本机（2026-11-23）
claude 2.1.330、codex 0.171.0、pi 1.6.0、kimi 2.1.1（最新 2.7.2），都是 npm 全局（前缀 ~/.npm-global）；hermes、gemini、opencode、agy 未安装。

## 未确认
- `agy update` 子命令是否存在；Hermes、Gemini、OpenCode、Antigravity 的 `--version` 输出格式。
- pi curl 安装时链接放在 PATH 哪个目录。
- homebrew-core 的 pi-coding-agent、kimi-code、hermes-agent 是否算官方支持（Hermes 文档把 Homebrew 版列为不支持的 Python 构建）。
- npm 原地覆盖对正在运行的 Node CLI 的实际影响（推断：延迟加载的代码块可能读到新版文件而崩）。
