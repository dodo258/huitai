//! 只读文件头尾：开头一段、结尾一段，切成完整的行；小文件整个读。
//! 大文件需要更多内容时（预览、精确计数）由调用方按需再读。

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::SystemTime;

/// 扫描时读的开头字节数：cwd、会话头都在这里。
pub(crate) const HEAD_BYTES: u64 = 64 * 1024;
/// 扫描时读的结尾字节数：标题、分支、最后时间都在这里。
pub(crate) const TAIL_BYTES: u64 = 64 * 1024;
/// 第一行比开头一段还长（Codex 的 session_meta 带整段系统提示）时，开头最多扩到原来的几倍。
const HEAD_GROW_MAX: u64 = 16;
/// 最后几行很长（大段工具输出）时，结尾最多扩到原来的几倍。
const TAIL_GROW_MAX: u64 = 64;
/// 预览往回读的上限。
pub(crate) const PREVIEW_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 文件头尾两段。`whole` 为真时 `head` 就是整个文件，`tail` 为空。
/// 内存有上限：开头最多 `head_bytes × 16`（默认 1MB）、结尾最多 `tail_bytes × 64`（默认 4MB），与文件多大无关。
pub(crate) struct Window {
    pub whole: bool,
    pub len: u64,
    pub mtime: SystemTime,
    head: Vec<u8>,
    tail: Vec<u8>,
}

impl Window {
    pub fn read(path: &Path) -> io::Result<Self> {
        Self::read_with(path, HEAD_BYTES, TAIL_BYTES)
    }

    /// 读开头 `head_bytes`、结尾 `tail_bytes`。开头一段里一个完整行都没有时往后扩（每次乘 4）；
    /// 结尾一段里除最后一行外的完整行不到 `tail_bytes / 2` 时往前扩。两段碰上了就整个读。
    pub fn read_with(path: &Path, head_bytes: u64, tail_bytes: u64) -> io::Result<Self> {
        let mut f = File::open(path)?;
        let meta = f.metadata()?;
        let len = meta.len();
        let mtime = meta.modified()?;
        let whole = |f: &mut File| -> io::Result<Self> {
            f.seek(SeekFrom::Start(0))?;
            let mut head = Vec::with_capacity(len as usize);
            f.take(len).read_to_end(&mut head)?;
            Ok(Self {
                whole: true,
                len,
                mtime,
                head,
                tail: Vec::new(),
            })
        };
        if len <= head_bytes + tail_bytes {
            return whole(&mut f);
        }

        let mut head = Vec::new();
        let mut want = head_bytes;
        loop {
            let more = want.min(len) - head.len() as u64;
            (&mut f).take(more).read_to_end(&mut head)?;
            let grown = want >= head_bytes * HEAD_GROW_MAX;
            if head.contains(&b'\n') || grown || head.len() as u64 >= len {
                break;
            }
            want *= 4;
        }

        let mut want = tail_bytes;
        loop {
            if head.len() as u64 + want >= len {
                return whole(&mut f);
            }
            let tail = read_tail(&mut f, len, want)?;
            let before_last = before_last_line(complete_suffix(&tail)).len() as u64;
            if before_last >= tail_bytes / 2 || want >= tail_bytes * TAIL_GROW_MAX {
                return Ok(Self {
                    whole: false,
                    len,
                    mtime,
                    head,
                    tail,
                });
            }
            want *= 4;
        }
    }

    /// 开头的完整行（整读时是全部行）。
    pub fn head_lines(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        let head = if self.whole {
            &self.head[..]
        } else {
            complete_prefix(&self.head)
        };
        lines(head)
    }

    /// 结尾的完整行（整读时是全部行）。
    pub fn tail_lines(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        let tail = if self.whole {
            &self.head[..]
        } else {
            complete_suffix(&self.tail)
        };
        lines(tail)
    }
}

/// 读文件最后 `bytes` 字节，丢掉开头不完整的那一行。返回内容和是否已读到文件开头。
pub(crate) fn read_tail_lines(path: &Path, bytes: u64) -> io::Result<(Vec<u8>, bool)> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let buf = read_tail(&mut f, len, bytes)?;
    if bytes >= len {
        Ok((buf, true))
    } else {
        Ok((complete_suffix(&buf).to_vec(), false))
    }
}

/// 逐行读的单行上限：比它长的行（几百 MB 不换行的坏数据）跳过不读进内存。
/// 正常一行最长是贴图的 base64，几 MB。
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// 从 `start` 字节起逐行读到文件尾（按需精确计数用），每行交给 `f(行, 是否以换行结尾)`；
/// 只有最后一行可能没有换行（还在写）。超过 [`MAX_LINE_BYTES`] 的行跳过，内存有上限。
/// 返回最后一个换行之后的位置：完整的行读到了哪里。
pub(crate) fn for_each_line_from(
    file: &mut File,
    start: u64,
    f: impl FnMut(&[u8], bool),
) -> io::Result<u64> {
    for_each_line_capped(file, start, MAX_LINE_BYTES, f)
}

fn for_each_line_capped(
    file: &mut File,
    start: u64,
    max: usize,
    mut f: impl FnMut(&[u8], bool),
) -> io::Result<u64> {
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    let (mut pos, mut complete_end) = (start, start);
    // 当前这一行已经超长，丢到下一个换行为止。
    let mut skipping = false;
    loop {
        let buf = match reader.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            if !skipping {
                emit(&line, false, &mut f);
            }
            return Ok(complete_end);
        }
        let (chunk, ends) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (&buf[..i], true),
            None => (buf, false),
        };
        if !skipping {
            if line.len() + chunk.len() > max {
                skipping = true;
                line = Vec::new();
            } else {
                line.extend_from_slice(chunk);
            }
        }
        let used = chunk.len() + usize::from(ends);
        reader.consume(used);
        pos += used as u64;
        if ends {
            if !skipping {
                emit(&line, true, &mut f);
            }
            line.clear();
            skipping = false;
            complete_end = pos;
        }
    }
}

/// 认文件用的摘要取开头和接缝处各多少字节。
const DIGEST_BYTES: u64 = 4096;

/// 文件前 `pos` 字节的摘要（FNV-1a）：`salt`、开头一段、`pos` 之前一段。
/// 文件只在 `pos` 之后追加时它不变；变短到 `pos` 以内要调用方先比长度。
pub(crate) fn digest(file: &mut File, pos: u64, salt: u32) -> io::Result<u64> {
    let mut h = Fnv::default();
    h.write(&salt.to_le_bytes());
    h.write(&pos.to_le_bytes());
    let n = pos.min(DIGEST_BYTES);
    for from in [0, pos - n] {
        let mut buf = Vec::with_capacity(n as usize);
        file.seek(SeekFrom::Start(from))?;
        (&mut *file).take(n).read_to_end(&mut buf)?;
        if buf.len() as u64 != n {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        h.write(&buf);
    }
    Ok(h.0)
}

/// FNV-1a 64：算法固定，存进缓存的摘要换了编译器版本也不变（标准库的哈希不保证）。
struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

fn emit(line: &[u8], complete: bool, f: &mut impl FnMut(&[u8], bool)) {
    let l = trim_line(line);
    if !l.is_empty() {
        f(l, complete);
    }
}

pub(crate) fn lines(buf: &[u8]) -> impl DoubleEndedIterator<Item = &[u8]> {
    buf.split(|&b| b == b'\n')
        .map(trim_line)
        .filter(|l| !l.is_empty())
}

fn read_tail(f: &mut File, len: u64, bytes: u64) -> io::Result<Vec<u8>> {
    let start = len.saturating_sub(bytes);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.take(len - start).read_to_end(&mut buf)?;
    Ok(buf)
}

fn trim_line(l: &[u8]) -> &[u8] {
    let l = l.strip_suffix(b"\n").unwrap_or(l);
    l.strip_suffix(b"\r").unwrap_or(l)
}

/// 去掉最后一个换行之后的半行。
fn complete_prefix(buf: &[u8]) -> &[u8] {
    match buf.iter().rposition(|&b| b == b'\n') {
        Some(i) => &buf[..i],
        None => &[],
    }
}

/// 去掉最后一个完整行（`buf` 以换行结尾时，最后一个换行之前的那一行）。
fn before_last_line(buf: &[u8]) -> &[u8] {
    let body = buf.strip_suffix(b"\n").unwrap_or(buf);
    match body.iter().rposition(|&b| b == b'\n') {
        Some(i) => &body[..=i],
        None => &[],
    }
}

/// 去掉第一个换行之前的半行。
fn complete_suffix(buf: &[u8]) -> &[u8] {
    match buf.iter().position(|&b| b == b'\n') {
        Some(i) => &buf[i + 1..],
        None => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str, content: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("huitai-window-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn small_file_is_read_whole() {
        let p = tmp("small", b"a\nb\r\nc");
        let w = Window::read(&p).unwrap();
        assert!(w.whole);
        let all: Vec<_> = w.head_lines().collect();
        assert_eq!(all, [&b"a"[..], b"b", b"c"]);
        assert_eq!(w.tail_lines().count(), 3);
    }

    #[test]
    fn large_file_keeps_only_complete_lines() {
        // 10 行，每行 8 字节 + 换行；头 25 字节、尾 30 字节。
        let content: Vec<u8> = (0..10)
            .flat_map(|i| format!("line-{i:03}\n").into_bytes())
            .collect();
        let p = tmp("large", &content);
        let w = Window::read_with(&p, 25, 30).unwrap();
        assert!(!w.whole);
        let head: Vec<_> = w.head_lines().collect();
        assert_eq!(head, [&b"line-000"[..], b"line-001"]);
        let tail: Vec<_> = w.tail_lines().collect();
        assert_eq!(tail, [&b"line-007"[..], b"line-008", b"line-009"]);
    }

    #[test]
    fn long_first_line_grows_head_within_cap() {
        let mut content = vec![b'h'; 100];
        content.push(b'\n');
        for i in 0..100 {
            content.extend_from_slice(format!("line-{i:03}\n").as_bytes());
        }
        let p = tmp("longhead", &content);
        let w = Window::read_with(&p, 20, 20).unwrap();
        assert!(!w.whole);
        assert_eq!(w.head_lines().next().map(<[u8]>::len), Some(100));
        // 超过上限（20 × 16）的第一行就不要了，只是不 panic、不越读越多。
        let mut content = vec![b'h'; 1000];
        content.extend_from_slice(b"\nrest\n");
        content.extend(std::iter::repeat_n(b'.', 2000));
        let p = tmp("hugehead", &content);
        let w = Window::read_with(&p, 20, 20).unwrap();
        assert!(w.head.len() <= 320);
        assert_eq!(w.head_lines().count(), 0);
    }

    #[test]
    fn long_last_line_grows_tail_to_keep_earlier_lines() {
        let mut content = Vec::new();
        for i in 0..200 {
            content.extend_from_slice(format!("line-{i:03}\n").as_bytes());
        }
        content.extend(std::iter::repeat_n(b'z', 300));
        content.push(b'\n');
        let p = tmp("longtail", &content);
        let w = Window::read_with(&p, 20, 20).unwrap();
        assert!(!w.whole);
        let tail: Vec<_> = w.tail_lines().collect();
        assert_eq!(tail.last().map(|l| l.len()), Some(300));
        assert!(tail.contains(&&b"line-199"[..]));
        assert!(w.tail.len() as u64 <= 20 * TAIL_GROW_MAX);
    }

    #[test]
    fn tail_reader_drops_partial_first_line() {
        let p = tmp("tail", b"aaaa\nbbbb\ncccc\n");
        let (buf, from_start) = read_tail_lines(&p, 8).unwrap();
        assert!(!from_start);
        assert_eq!(lines(&buf).collect::<Vec<_>>(), [&b"cccc"[..]]);
        let (buf, from_start) = read_tail_lines(&p, 100).unwrap();
        assert!(from_start);
        assert_eq!(lines(&buf).count(), 3);
    }

    #[test]
    fn overlong_lines_are_skipped_not_buffered() {
        let mut content = b"short-1\n".to_vec();
        content.extend(std::iter::repeat_n(b'x', 5000));
        content.extend_from_slice(b"\nshort-2\r\n\n");
        content.extend(std::iter::repeat_n(b'y', 5000));
        let p = tmp("overlong", &content);
        let mut got = Vec::new();
        let mut f = File::open(&p).unwrap();
        let end = for_each_line_capped(&mut f, 0, 100, |l, _| got.push(l.to_vec())).unwrap();
        assert_eq!(got, [b"short-1".to_vec(), b"short-2".to_vec()]);
        assert_eq!(end, content.len() as u64 - 5000);
        // 上限内的长行、没有结尾换行的最后一行照常给出（标成没写完）。
        let mut got = Vec::new();
        for_each_line_capped(&mut f, 0, 6000, |l, done| got.push((l.len(), done))).unwrap();
        assert_eq!(got, [(7, true), (5000, true), (7, true), (5000, false)]);
        // 从中间一行开头接着读。
        let mut got = Vec::new();
        for_each_line_capped(&mut f, 8, 6000, |l, _| got.push(l.len())).unwrap();
        assert_eq!(got, [5000, 7, 5000]);
    }

    #[test]
    fn empty_file() {
        let p = tmp("empty", b"");
        let w = Window::read(&p).unwrap();
        assert!(w.whole);
        assert_eq!(w.head_lines().count(), 0);
        let mut n = 0;
        let end = for_each_line_from(&mut File::open(&p).unwrap(), 0, |_, _| n += 1).unwrap();
        assert_eq!((n, end), (0, 0));
    }

    #[test]
    fn digest_sees_head_and_seam_but_not_appends() {
        let body: Vec<u8> = (0..2000)
            .flat_map(|i| format!("line-{i:05}\n").into_bytes())
            .collect();
        let p = tmp("digest", &body);
        let d = |p: &std::path::Path, pos, salt| digest(&mut File::open(p).unwrap(), pos, salt);
        let pos = 1000 * 11;
        let base = d(&p, pos, 1).unwrap();
        // 后面追加、改 pos 之后的内容：不变。
        let mut more = body.clone();
        more.extend_from_slice(b"appended\n");
        more[pos as usize + 3] = b'#';
        fs::write(&p, &more).unwrap();
        assert_eq!(d(&p, pos, 1).unwrap(), base);
        // 改开头、改接缝处、换 salt、换位置：都变。
        for at in [5, pos as usize - 2] {
            let mut bad = body.clone();
            bad[at] = b'#';
            fs::write(&p, &bad).unwrap();
            assert_ne!(d(&p, pos, 1).unwrap(), base, "改了第 {at} 字节");
        }
        fs::write(&p, &body).unwrap();
        assert_ne!(d(&p, pos, 2).unwrap(), base);
        assert_ne!(d(&p, pos - 11, 1).unwrap(), base);
        // 文件比 pos 短：报错，不当成一样。
        assert!(d(&p, body.len() as u64 + 1, 1).is_err());
    }
}
