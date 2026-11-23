//! 找本机装着哪几家：`ai doctor` 问登录 shell 要 PATH 和 `npm prefix -g`（和开对话时一样用 `$SHELL -lc`），
//! 会话台界面里用自己的 PATH（[`current_env`]）。在 PATH 里按顺序找命令（不用 `command -v`：命令被起了别名时它给的是别名），再认装法、补版本。

use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::core::{Vendor, exec};

use super::detect::{self, Env, Install};

/// 登录 shell 最多等多久（nvm、oh-my-zsh 这类配置启动慢）。
pub const SHELL_TIMEOUT: Duration = Duration::from_secs(15);
/// 问 `--version` 最多等多久。
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

const PATH_MARK: &str = "HUITAI_PATH=";
const NPM_MARK: &str = "HUITAI_NPM=";

/// 登录 shell 里跑的那一行。标记前是 shell 配置自己打印的东西。
pub fn login_script() -> String {
    format!(
        "printf '\\n{PATH_MARK}%s\\n' \"$PATH\"; \
         printf '{NPM_MARK}%s\\n' \"$(npm prefix -g 2>/dev/null)\"; exit 0"
    )
}

/// 登录 shell 里的 PATH 和 npm 前缀。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginEnv {
    pub path: Vec<PathBuf>,
    pub npm_prefix: Option<PathBuf>,
}

/// 解析 [`login_script`] 的输出；没有标记行（shell 没跑到那里）是 `None`。
pub fn parse_login(out: &str) -> Option<LoginEnv> {
    let mut path = None;
    let mut npm = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix(PATH_MARK) {
            path = Some(p.trim().to_owned());
        } else if let Some(p) = line.strip_prefix(NPM_MARK) {
            npm = Some(p.trim().to_owned());
        }
    }
    Some(LoginEnv {
        path: std::env::split_paths(&path?)
            .filter(|p| p.is_absolute())
            .collect(),
        npm_prefix: npm.filter(|p| !p.is_empty()).map(PathBuf::from),
    })
}

/// 起登录 shell 取 [`LoginEnv`]。
pub fn login_env(shell: &Path) -> io::Result<LoginEnv> {
    let out = exec::run(shell, &["-lc", &login_script()], None, SHELL_TIMEOUT)?;
    parse_login(&out).ok_or_else(|| io::Error::other("登录 shell 没打印出 PATH"))
}

/// 会话台界面里用：不起登录 shell（`.zprofile` 里要是有读写终端的命令会打乱界面），直接用本进程的 PATH
/// （从开会话台的终端标签页继承，和登录 shell 的基本一样），npm 前缀跑一次 `npm prefix -g`。
pub fn current_env() -> LoginEnv {
    let path = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .filter(|d| d.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    let npm_prefix = exec::run(
        "/usr/bin/env",
        &["npm", "prefix", "-g"],
        None,
        VERSION_TIMEOUT,
    )
    .ok()
    .map(|out| out.trim().to_owned())
    .filter(|p| !p.is_empty())
    .map(PathBuf::from);
    LoginEnv { path, npm_prefix }
}

/// 在 `dirs` 里按顺序找第一个叫 `program` 的可执行文件。
pub fn which(program: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    dirs.iter().map(|d| d.join(program)).find(|p| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

/// PATH 上所有叫 `program` 的可执行文件，按顺序（同一份经不同目录找到的只算一次）。升级完复查有没有被前面的遮住时用。
pub fn which_all(program: &str, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut real: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if let Some(p) = which(program, std::slice::from_ref(d)) {
            let r = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if !real.contains(&r) {
                real.push(r);
                out.push(p);
            }
        }
    }
    out
}

/// 认一个指定位置的命令：认装法，路径里读不出版本就问 `--version`。
pub fn probe_one(vendor: Vendor, path: &Path, login: &LoginEnv, home: &Path) -> Install {
    let env = Env {
        home: home.to_owned(),
        npm_prefix: login.npm_prefix.clone(),
    };
    let mut install = detect::detect(vendor, path, &env);
    if install.version.is_none() {
        let path_var = std::env::join_paths(&login.path).ok();
        install.version = ask_version(&install.path, path_var.as_deref());
    }
    install
}

/// 每家找一遍：找到的认装法；路径里读不出版本的，并行问 `--version`（每个最多 10 秒，问不出就空着）。
/// 返回顺序和 [`Vendor::ALL`] 一致，没装的不在里面。
pub fn probe(login: &LoginEnv, home: &Path) -> Vec<Install> {
    let env = Env {
        home: home.to_owned(),
        npm_prefix: login.npm_prefix.clone(),
    };
    let found: Vec<Install> = Vendor::ALL
        .iter()
        .filter_map(|&v| {
            which(detect::program(v), &login.path).map(|p| detect::detect(v, &p, &env))
        })
        .collect();
    let path_var = std::env::join_paths(&login.path).ok();
    thread::scope(|s| {
        let jobs: Vec<_> = found
            .into_iter()
            .map(|mut install| {
                let path_var = path_var.clone();
                s.spawn(move || {
                    if install.version.is_none() {
                        install.version = ask_version(&install.path, path_var.as_deref());
                    }
                    install
                })
            })
            .collect();
        jobs.into_iter().filter_map(|j| j.join().ok()).collect()
    })
}

/// 跑 `<命令> --version`（用登录 shell 的 PATH，`#!/usr/bin/env node` 一类脚本才找得到解释器）。
fn ask_version(program: &Path, path_var: Option<&std::ffi::OsStr>) -> Option<String> {
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    if let Some(p) = path_var {
        let mut kv = std::ffi::OsString::from("PATH=");
        kv.push(p);
        args.push(kv);
    }
    args.push(program.as_os_str().to_owned());
    args.push("--version".into());
    let out = exec::run("/usr/bin/env", &args, None, VERSION_TIMEOUT).ok()?;
    detect::version_in(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_output_skips_shell_noise() {
        let out =
            format!("欢迎\n\n{PATH_MARK}/a/bin:relative:/b/bin\n{NPM_MARK}/Users/me/.npm-global\n");
        let env = parse_login(&out).unwrap();
        assert_eq!(
            env.path,
            vec![PathBuf::from("/a/bin"), PathBuf::from("/b/bin")]
        );
        assert_eq!(env.npm_prefix, Some(PathBuf::from("/Users/me/.npm-global")));
        let none = parse_login(&format!("{PATH_MARK}/a\n{NPM_MARK}\n")).unwrap();
        assert_eq!(none.npm_prefix, None);
        assert_eq!(parse_login("没有标记"), None);
    }

    #[test]
    fn login_script_runs_in_a_real_shell() {
        let out = exec::run("/bin/sh", &["-c", &login_script()], None, SHELL_TIMEOUT).unwrap();
        let env = parse_login(&out).unwrap();
        assert!(which("sh", &env.path).is_some(), "{out}");
        assert_eq!(which("huitai-no-such-cli", &env.path), None);
    }
}
