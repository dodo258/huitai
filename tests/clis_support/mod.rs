//! 查最新版的假 HTTP 服务（127.0.0.1）。
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// 路径 → (状态码, 正文, ETag)；状态码 0 表示不回应。
type Routes = Arc<Mutex<HashMap<String, (u16, String, String)>>>;

/// 假服务，记下每次请求的路径和 If-None-Match。
#[derive(Clone)]
pub struct Fake {
    pub origin: String,
    routes: Routes,
    /// 收到的请求：（路径，If-None-Match）。
    pub seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Fake {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let fake = Fake {
            origin: format!("http://{}", listener.local_addr().unwrap()),
            routes: Arc::default(),
            seen: Arc::default(),
        };
        let f = fake.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let f = f.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    reader.read_line(&mut first).unwrap();
                    let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let mut if_none = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.strip_prefix("If-None-Match: ") {
                            if_none = v.trim().to_owned();
                        }
                    }
                    f.seen.lock().unwrap().push((path.clone(), if_none.clone()));
                    let (code, body, etag) =
                        f.routes.lock().unwrap().get(&path).cloned().unwrap_or((
                            404,
                            "{}".to_owned(),
                            String::new(),
                        ));
                    if code == 0 {
                        thread::sleep(Duration::from_secs(5));
                        return;
                    }
                    let (code, body) = if !etag.is_empty() && if_none == etag {
                        (304, String::new())
                    } else {
                        (code, body)
                    };
                    let etag_line = if etag.is_empty() {
                        String::new()
                    } else {
                        format!("ETag: {etag}\r\n")
                    };
                    let mut stream = stream;
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {code} X\r\n{etag_line}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        fake
    }

    pub fn route(&self, path: &str, code: u16, body: &str) {
        self.route_etag(path, code, body, "");
    }

    pub fn route_etag(&self, path: &str, code: u16, body: &str, etag: &str) {
        self.routes
            .lock()
            .unwrap()
            .insert(path.to_owned(), (code, body.to_owned(), etag.to_owned()));
    }

    pub fn hits(&self, path: &str) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, e)| e.clone())
            .collect()
    }
}
