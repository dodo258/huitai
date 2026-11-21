//! 别的进程开着的对话：从本机进程的命令行里认出带着会话 id 的三家 CLI。
//! 桌面端、别的终端、后台任务开着一条对话时，CLI 进程的参数里带着它的 id
//! （`claude --resume=<id>`、`claude -r <id>`、`claude --session-id <id>`、`codex resume <id>`）；
//! 对话在等工具或等你回复时会话文件可以几分钟不动，只看文件写入时间会误判成「已停」。

use std::collections::HashSet;

/// `ps -axww -o command=` 的输出里，紧跟在会话参数后面的 id。只认参数位置，不在整行里找，
/// 免得把提示词里提到的 id 也当成开着。
pub fn session_ids(ps_output: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for line in ps_output.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (i, w) in words.iter().enumerate() {
            let id = match w.split_once('=') {
                Some(("--resume" | "--session-id", v)) => Some(v),
                _ if matches!(*w, "--resume" | "-r" | "--session-id" | "resume") => {
                    words.get(i + 1).copied()
                }
                _ => None,
            };
            if let Some(id) = id.filter(|id| is_uuid(id)) {
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
