# 安全 / Security

会话台的守护进程只在本机监听一个 Unix socket（`~/Library/Application Support/huitai/` 下，只有当前用户能读写），不开网络端口。联网只有两处，见 README「隐私」一节。

发现安全问题请不要公开提 Issue，用 GitHub 的 [私密报告](https://github.com/dodo258/huitai/security/advisories/new) 告诉我们：写清版本（`ai --version`）、怎么复现、影响是什么。收到后会尽快回复，修好之前不公开细节。

The daemon listens only on a local Unix socket under `~/Library/Application Support/huitai/` that only your user can access; it opens no network ports. Network access is limited to the two cases in the README's Privacy section. Please report security issues privately through [GitHub security advisories](https://github.com/dodo258/huitai/security/advisories/new), not as public issues, with the version (`ai --version`), steps to reproduce and the impact.
