//! 别的进程开着的对话：从本机进程的命令行里认出带着会话 id 的各家 CLI。
//! 桌面端、别的终端、后台任务开着一条对话时，CLI 进程的参数里带着它的 id
//! （`claude --resume=<id>`、`claude -r <id>`、`claude --session-id <id>`、`codex resume <id>`、`hermes -r <id>`、
//! `kimi -S <id>`、`opencode <目录> -s <id>`）；
//! 对话在等工具或等你回复时会话文件可以几分钟不动，只看文件写入时间会误判成「已停」。

use std::collections::HashSet;

use super::opencode;

/// `ps -axww -o command=` 的输出里，紧跟在会话参数后面的 id。只认参数位置，不在整行里找，
/// 免得把提示词里提到的 id 也当成开着。
pub fn session_ids(ps_output: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in ps_output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (i, w) in words.iter().enumerate() {
            let id = match w.split_once('=') {
                Some(("--resume" | "--session-id" | "--session", v)) => Some(v),
                _ if matches!(
                    *w,
                    "--resume" | "-r" | "--session-id" | "resume" | "-S" | "-s" | "--session"
                ) =>
                {
                    words.get(i + 1).copied()
                }
                _ => None,
            };
            let Some(id) = id else { continue };
            if opencode::is_session_id(id) {
                // OpenCode 的 id 区分大小写（base62），原样记。
                out.insert(id.to_owned());
            } else if *w != "-s" && (is_uuid(id) || is_hermes_id(id) || is_kimi_id(id)) {
                // `-s` 只是 OpenCode 的，别的程序 `-s <uuid>` 不算。
                out.insert(id.to_ascii_lowercase());
            }
        }
    }
    out
}

/// 8-4-4-4-12 位十六进制。
fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Hermes 的 `YYYYMMDD_HHMMSS_<十六进制>`。
fn is_hermes_id(s: &str) -> bool {
    let parts: Vec<&str> = s.split('_').collect();
    matches!(parts.as_slice(), [date, time, hex]
        if date.len() == 8 && time.len() == 6 && !hex.is_empty()
            && date.bytes().chain(time.bytes()).all(|b| b.is_ascii_digit())
            && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Kimi Code 的 `session_<uuid>`。
fn is_kimi_id(s: &str) -> bool {
    s.strip_prefix("session_").is_some_and(is_uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "ba69ce37-ca56-4620-ad79-98e825c5a39d";
    const B: &str = "c3fac37e-660c-40ad-9796-289eaf99d49c";
    const C: &str = "019a1b2c-3d4e-7f80-9a1b-2c3d4e5f6a7b";

    #[test]
    fn finds_ids_after_session_flags() {
        let ps = format!(
            "/Applications/Claude.app/x/claude --output-format stream-json --resume={A} --model opus\n\
             /usr/local/bin/claude --session-id {B} -p 做点事\n\
             codex resume {C}\n\
             /bin/zsh -l\n"
        );
        let ids = session_ids(&ps);
        assert_eq!(ids, [A, B, C].map(str::to_owned).into());
    }

    #[test]
    fn hermes_resume() {
        let ids =
            session_ids("/usr/local/bin/hermes -r 20261120_101500_a1b2c3\nhermes -r latest\n");
        assert_eq!(ids, ["20261120_101500_a1b2c3".to_owned()].into());
    }

    #[test]
    fn opencode_session_flag_keeps_case() {
        let id = "ses_7f3a1b2c3d4eAbCdEfGhIjKlMn";
        let ids = session_ids(&format!(
            "/opt/homebrew/bin/opencode /work/api -s {id}\nopencode --session={id}x\nls -s 4\nfoo -s {A}\n"
        ));
        assert_eq!(ids, [id.to_owned(), format!("{id}x")].into());
    }

    #[test]
    fn kimi_session() {
        let id = format!("session_{A}");
        let ps = format!(
            "node /Users/me/.npm-global/bin/kimi -S {id}\n\
             kimi --session=session_{B}\n\
             kimi -S\n\
             tmux -S /tmp/sock\n"
        );
        assert_eq!(session_ids(&ps), [id, format!("session_{B}")].into());
    }

    #[test]
    fn short_flag_and_uppercase() {
        let ids = session_ids(&format!("claude -r {}", A.to_uppercase()));
        assert!(ids.contains(A));
    }

    #[test]
    fn ignores_ids_elsewhere_in_the_line() {
        // 提示词里提到的 id、缺值的参数、不是 id 的值都不算。
        let ps = format!(
            "claude -p 接着看 {A} 那条\n\
             claude --resume\n\
             claude --resume=latest\n\
             git log {B}\n"
        );
        assert!(session_ids(&ps).is_empty());
    }
}
