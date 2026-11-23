//! 系统自带 curl 的命令行（跑命令用 [`crate::core::exec`]）。

/// 系统自带的 curl 的完整命令（程序在第一个）：不走 PATH 里别处装的 curl；第一个参数固定是 `-q`，不读
/// `~/.curlrc`（那里的 `--trace-ascii` 之类会把经标准输入给的鉴权头写进文件，`-q` 只有放在第一个才生效）。
pub(super) fn argv<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut argv = vec!["/usr/bin/curl", "-q"];
    argv.extend_from_slice(args);
    argv
}
