//! 用系统自带的 `curl` 发一个 GET。全项目查版本、下载新版都走这一份。
//!
//! `-q` 必须第一个：不读 `~/.curlrc`，免得带上用户自己配的头或凭据；只发调用方给的头，不带任何用户信息。
//! 超时由 curl 的 `--max-time` 管，外面再由 [`super::exec::run`] 多留几秒兜底。

use std::path::Path;
use std::time::Duration;

use super::exec;

/// 跟在正文后面的标记：`-w` 在它后面写状态码和 ETag，正文里不会有这个控制字符开头的整串。
const MARK: &str = "\u{1f}huitai-http ";

/// 一次请求的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// 最后一跳的状态码（跟了重定向的话）。304 表示 ETag 没变，正文是空的。
    pub status: u16,
    /// 正文；`out` 写进文件时是空的。
    pub body: String,
    /// 响应里的 ETag，下次带着它问（`If-None-Match`）没变就回 304。
    pub etag: Option<String>,
}

/// GET `url`，`headers` 是一行行 `名字: 值`；`out` 给了就把正文写进那个文件。
/// 连不上、超时、状态码 400 起都算失败，错误里带上原因。
pub fn get(
    url: &str,
    headers: &[&str],
    out: Option<&Path>,
    timeout: Duration,
) -> Result<Response, String> {
    let max = timeout.as_secs().max(1).to_string();
    let write_out = format!("{MARK}%{{http_code}} %header{{etag}}");
    let mut args = vec![
        "-q",
        "-sSL",
        "--proto",
        "=https,http",
        "--max-time",
        &max,
        "--noproxy",
        "127.0.0.1,localhost",
        "-w",
        &write_out,
    ];
    for h in headers {
        args.extend(["-H", h]);
    }
    let out = out.map(|p| p.to_string_lossy().into_owned());
    if let Some(p) = &out {
        args.extend(["-o", p]);
    }
    args.push(url);
    let raw = exec::run(
        Path::new("/usr/bin/curl"),
        &args,
        None,
        timeout + Duration::from_secs(5),
    )
    .map_err(|e| e.to_string())?;
    let response = parse(&raw).ok_or_else(|| "curl 的输出看不懂".to_owned())?;
    if response.status >= 400 || response.status == 0 {
        return Err(format!("服务器回了 HTTP {}", response.status));
    }
    Ok(response)
}

/// 拆开「正文 + 标记 + 状态码 + 空格 + ETag」。
fn parse(raw: &str) -> Option<Response> {
    let (body, tail) = raw.rsplit_once(MARK)?;
    let (code, etag) = tail.split_once(' ').unwrap_or((tail, ""));
    let etag = etag.trim();
    Some(Response {
        status: code.trim().parse().ok()?,
        body: body.to_owned(),
        // 老版本 curl 不认 `%header{}` 会原样印出来：只收像 ETag 的值。
        etag: (etag.starts_with('"') || etag.starts_with("W/\"")).then(|| etag.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_body_status_and_etag() {
        let r = parse(&format!("{{\"a\":1}}\n{MARK}200 W/\"abc\"")).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, "{\"a\":1}\n");
        assert_eq!(r.etag.as_deref(), Some("W/\"abc\""));
        let r = parse(&format!("{MARK}304 ")).unwrap();
        assert_eq!((r.status, r.body.as_str(), r.etag), (304, "", None));
        assert_eq!(
            parse(&format!("x{MARK}200 %header{{etag}}")).unwrap().etag,
            None
        );
        assert_eq!(parse("没有标记"), None);
    }
}
