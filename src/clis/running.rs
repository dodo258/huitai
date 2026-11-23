//! 本机有没有这家 CLI 的进程在跑（不管是不是会话台托管的）：`ps` 列出全部进程，按程序所在的路径认是不是
//! 要升的那一份。桌面版、编辑器插件自带的那份路径不同，不算。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::core::{Vendor, exec};

use super::detect::{self, Install};

/// `ps` 最多等多久。
const PS_TIMEOUT: Duration = Duration::from_secs(10);
/// 跑脚本的解释器（`node /…/bin/codex`）：程序路径要看它的第一个参数。
const INTERPRETERS: [&str; 4] = ["node", "bun", "deno", "python"];

/// 本机全部进程，每行「进程号 命令行」。
pub fn processes() -> io::Result<String> {
    exec::run(
        "/bin/ps",
        &["-axww", "-o", "pid=,command="],
        None,
        PS_TIMEOUT,
    )
}

/// `ps`（[`processes`] 的输出）里在用 `install` 这一份的进程号。
pub(super) fn pids<'a>(ps: &'a str, install: &Install, home: &Path) -> Vec<&'a str> {
    let files = Files {
        path: &install.path,
        replaced: install.replaced(home),
        names: [detect::program(install.vendor), alias(install.vendor)],
    };
    ps.lines()
        .filter_map(|line| line.trim_start().split_once(char::is_whitespace))
        .filter(|(_, command)| files.run_by(command.trim_start()))
        .map(|(pid, _)| pid)
        .collect()
}

/// `ps` 里看不出路径时，除了命令名还会是什么：自己改的进程名（Node 的 `process.title` 会盖掉整条命令行），
/// 或 `python -m` 的模块名。
fn alias(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::Pi => "pi-rpc",
        Vendor::Kimi => "kimi-code",
        Vendor::Hermes => "hermes_cli",
        v => detect::program(v),
    }
}

/// 要升的那一份在哪、叫什么。
struct Files<'a> {
    path: &'a Path,
    replaced: PathBuf,
    names: [&'static str; 2],
}

impl Files<'_> {
    fn run_by(&self, command: &str) -> bool {
        if self.starts(command) {
            return true;
        }
        let mut rest = command;
        let interpreter = name(next_word(&mut rest)).to_ascii_lowercase();
        if !INTERPRETERS.iter().any(|i| interpreter.starts_with(i)) {
            return false;
        }
        if !rest.starts_with('-') {
            return self.starts(rest);
        }
        // 解释器带着选项：有的后面跟值（`-r <文件>`）、有的后面是整段代码（`-c <代码>`），`ps` 里空格分不开，
        // 说不准哪个是脚本。后面提到要升的那一份就当在跑。
        let args = rest;
        while rest.starts_with('-') {
            next_word(&mut rest);
        }
        self.starts(rest) || self.mentions(args)
    }

    /// 这段命令行开头的程序是不是要升的那一份。
    fn starts(&self, text: &str) -> bool {
        if text.starts_with('/') {
            return self.at(text);
        }
        // 终端里敲命令名起的，`ps` 只给名字、看不出路径：当作就是这一份。
        self.names.contains(&name(next_word(&mut { text })))
    }

    /// `text` 开头的绝对路径是不是在要升的那一份里。
    fn at(&self, text: &str) -> bool {
        // 路径里可能有空格，先拿知道的路径比整段开头，再按第一个词解开链接比。
        let known = [self.path, &self.replaced].iter().any(|p| {
            text.strip_prefix(&*p.to_string_lossy())
                .is_some_and(|after| after.starts_with('/') || !after.starts_with(in_path))
        });
        known
            || text
                .split(|c: char| c.is_whitespace() || "'\"(),;".contains(c))
                .next()
                .and_then(|word| fs::canonicalize(word).ok())
                .is_some_and(|real| real.starts_with(&self.replaced))
    }

    /// 解释器的参数里有没有提到要升的那一份：哪里写着它的路径（代码里引号包着的也算），或 `-m` 后面是它的模块。
    fn mentions(&self, args: &str) -> bool {
        let path = args
            .match_indices('/')
            .any(|(i, _)| !args[..i].ends_with(in_path) && self.at(&args[i..]));
        let words: Vec<&str> = args.split_whitespace().collect();
        path || words.windows(2).any(|w| {
            let module = w[1].split('.').next().unwrap_or(w[1]);
            let short = w[0].starts_with('-') && !w[0].starts_with("--");
            short && w[0].ends_with('m') && self.names.contains(&module)
        })
    }
}

/// 路径里会有的字：绝对路径的开头前面、已知路径的后面不能紧挨着这种字。
fn in_path(c: char) -> bool {
    c.is_alphanumeric() || "/._-~@+".contains(c)
}

/// 取走开头的一个词和它后面的空白。
fn next_word<'a>(text: &mut &'a str) -> &'a str {
    let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    *text = rest.trim_start();
    word
}

/// 路径的最后一段。
fn name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clis::detect::Method;

    const FRAMEWORK: &str = "/opt/homebrew/Cellar/python@3.14/3.14.5/Frameworks/Python.framework/Versions/3.14/Resources/Python.app/Contents/MacOS/Python";

    fn hermes() -> Install {
        Install {
            vendor: Vendor::Hermes,
            path: "/nowhere/brew/bin/hermes".into(),
            real: "/nowhere/brew/Cellar/hermes-agent/0.22.4/bin/hermes".into(),
            method: Method::Formula("hermes-agent".to_owned()),
            version: Some("0.22.4".to_owned()),
            problem: None,
        }
    }

    fn running(command: &str, install: &Install) -> bool {
        !pids(
            &format!("  77 {command}\n"),
            install,
            Path::new("/nowhere/home"),
        )
        .is_empty()
    }

    /// Hermes 常驻 gateway：框架版 python 的路径看不出是哪家，安装目录只写在 `-c` 的代码里；或者只有模块名。
    #[test]
    fn interpreter_options_that_take_a_value() {
        let h = hermes();
        let code = |dir: &str| {
            format!(
                "import os, sys, runpy; os.environ.pop('PYTHONPATH', None); sys.path.insert(0, '{dir}'); import hermes_bootstrap; runpy.run_module('hermes_cli.main', run_name='__main__', alter_sys=True) gateway run"
            )
        };
        let lib = "/nowhere/brew/Cellar/hermes-agent/0.22.4/libexec/lib/python3.14/site-packages";
        for yes in [
            format!("{FRAMEWORK} -I -c {}", code(lib)),
            format!("{FRAMEWORK} -Ic {}", code(lib)),
            format!(
                "python3 -I -c {}",
                code("/nowhere/brew/Cellar/hermes-agent")
            ),
            format!("{FRAMEWORK} -m hermes_cli.main gateway run"),
            format!("{FRAMEWORK} -Im hermes_cli.main gateway run"),
            format!("{FRAMEWORK} -X utf8 {lib}/run.py"),
            format!("node -r /srv/hook.js {lib}/cli.js"),
            format!("node --require=/srv/hook.js --import /srv/x.mjs {lib}/cli.js"),
        ] {
            assert!(running(&yes, &h), "{yes}");
        }
        for no in [
            format!("{FRAMEWORK} -I -c {}", code("/nowhere/other/hermes-agent")),
            format!(
                "{FRAMEWORK} -I -c {}",
                code("/nowhere/brew/Cellar/hermes-agent-x/lib")
            ),
            format!(
                "{FRAMEWORK} -I -c {}",
                code("/srv/nowhere/brew/Cellar/hermes-agent")
            ),
            format!("{FRAMEWORK} -m http.server 8000"),
            format!("{FRAMEWORK} -u /srv/bot.py hermes"),
            format!("{FRAMEWORK} /srv/bot.py --cli {lib}/cli.js"),
            format!("grep -c {lib}/cli.js"),
        ] {
            assert!(!running(&no, &h), "{no}");
        }
    }

    /// 代码里写的是经过链接的路径（Homebrew 的 `opt/<名>`）：解开链接再比。
    #[test]
    fn links_inside_inline_code_are_resolved() {
        let root = std::env::temp_dir().join(format!("huitai-running-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("Cellar/hermes-agent/0.22.4/libexec")).unwrap();
        let root = fs::canonicalize(&root).unwrap();
        std::os::unix::fs::symlink(root.join("Cellar/hermes-agent/0.22.4"), root.join("opt"))
            .unwrap();
        let h = Install {
            real: root.join("Cellar/hermes-agent/0.22.4/bin/hermes"),
            ..hermes()
        };
        let opt = root.join("opt/libexec");
        let found = running(
            &format!("{FRAMEWORK} -c sys.path.insert(0, \"{}\")", opt.display()),
            &h,
        );
        let _ = fs::remove_dir_all(&root);
        assert!(found);
    }
}
