//! 发送队列：要写给 PTY（输入）或客户端 socket（输出）的字节先排队，由各自的写线程写出去。
//!
//! 守护进程持着一条对话的锁时只往队列里放，从不直接写可能阻塞的描述符：
//! CLI 卡住不读输入、客户端卡住不读输出时，只卡住对应的写线程，读线程、别的请求和别的对话照常。

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// 队列满时 [`Outbox::push`] 每隔这么久问一次还要不要接着等。
const WAIT_STEP: Duration = Duration::from_millis(200);

pub struct Outbox {
    queue: Mutex<Queue>,
    /// 有新数据、有空位、关闭时都会通知。
    changed: Condvar,
    /// 排队字节数上限：[`push`](Self::push) 等、[`try_push`](Self::try_push) 拒收。
    limit: usize,
}

#[derive(Default)]
struct Queue {
    bufs: VecDeque<Vec<u8>>,
    bytes: usize,
    /// 不再接收；`drain` 为真时把已排队的写完再停，否则直接丢掉。
    closed: bool,
    drain: bool,
}

impl Outbox {
    /// 开一个写线程把排队的字节写进 `sink`。写失败或关闭后线程调用 `on_end` 再退出。
    pub fn start<W: Write + Send + 'static>(
        mut sink: W,
        limit: usize,
        on_end: impl FnOnce() + Send + 'static,
    ) -> Arc<Self> {
        let outbox = Arc::new(Self {
            queue: Mutex::default(),
            changed: Condvar::new(),
            limit,
        });
        let writer = Arc::clone(&outbox);
        thread::spawn(move || {
            while let Some(bytes) = writer.pop() {
                if sink.write_all(&bytes).is_err() {
                    writer.close();
                    break;
                }
            }
            on_end();
        });
        outbox
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fits(&self, q: &Queue, len: usize) -> bool {
        q.bytes == 0 || q.bytes + len <= self.limit
    }

    /// 排满时等着，每 [`WAIT_STEP`] 问一次 `keep_waiting`（不持有队列的锁）。
    /// 排进去返回 `true`；已关闭或 `keep_waiting` 说不等了返回 `false`，这批丢掉。
    pub fn push(&self, bytes: Vec<u8>, keep_waiting: impl Fn() -> bool) -> bool {
        loop {
            {
                let q = self.lock();
                if q.closed {
                    return false;
                }
                if self.fits(&q, bytes.len()) {
                    self.enqueue(q, bytes);
                    return true;
                }
                let _ = self
                    .changed
                    .wait_timeout(q, WAIT_STEP)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if !keep_waiting() {
                return false;
            }
        }
    }

    /// 不等：排满或已关闭就不收，返回 `false`。
    pub fn try_push(&self, bytes: Vec<u8>) -> bool {
        let q = self.lock();
        if q.closed || !self.fits(&q, bytes.len()) {
            return false;
        }
        self.enqueue(q, bytes);
        true
    }

    /// 不管上限都排进去（补发画面、结束原因这类必须送到的）；已关闭返回 `false`。
    pub fn force_push(&self, bytes: Vec<u8>) -> bool {
        let q = self.lock();
        if q.closed {
            return false;
        }
        self.enqueue(q, bytes);
        true
    }

    fn enqueue(&self, mut q: MutexGuard<'_, Queue>, bytes: Vec<u8>) {
        q.bytes += bytes.len();
        q.bufs.push_back(bytes);
        self.changed.notify_all();
    }

    /// 不再接收，丢掉没写的；写线程写完手上这一批后退出。
    pub fn close(&self) {
        let mut q = self.lock();
        q.closed = true;
        q.drain = false;
        q.bufs.clear();
        q.bytes = 0;
        self.changed.notify_all();
    }

    /// 不再接收，已排队的写完再让写线程退出。已经 [`close`](Self::close) 的不受影响。
    pub fn finish(&self) {
        let mut q = self.lock();
        if !q.closed {
            q.closed = true;
            q.drain = true;
        }
        self.changed.notify_all();
    }

    /// 等下一批；关闭（且不用写完剩下的，或已写完）后返回 `None`。
    fn pop(&self) -> Option<Vec<u8>> {
        let mut q = self.lock();
        loop {
            if q.closed && !q.drain {
                return None;
            }
            if let Some(bytes) = q.bufs.pop_front() {
                q.bytes -= bytes.len();
                self.changed.notify_all();
                return Some(bytes);
            }
            if q.closed {
                return None;
            }
            q = self.changed.wait(q).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Instant;

    const LIMIT: usize = 256 * 1024;

    /// 用 socket 对当写出去的地方：不读另一端时写会卡住，和 CLI 不读输入、客户端不读输出一样。
    fn start() -> (Arc<Outbox>, UnixStream, mpsc::Receiver<()>) {
        let (near, far) = UnixStream::pair().unwrap();
        let (tx, rx) = mpsc::channel();
        let outbox = Outbox::start(near, LIMIT, move || {
            let _ = tx.send(());
        });
        (outbox, far, rx)
    }

    /// 一直塞到排满，并且写线程已经卡在 socket 缓冲满上（停一会儿再塞也塞不进）。
    fn fill(outbox: &Outbox) {
        let chunk = vec![b'z'; 64 * 1024];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while outbox.try_push(chunk.clone()) {
                assert!(Instant::now() < deadline, "队列一直没排满");
            }
            thread::sleep(Duration::from_millis(100));
            if !outbox.try_push(chunk.clone()) {
                return;
            }
        }
    }

    #[test]
    fn writes_in_order_then_finishes() {
        let (outbox, mut far, ended) = start();
        assert!(outbox.push(b"ab".to_vec(), || true));
        assert!(outbox.try_push(b"cd".to_vec()));
        assert!(outbox.force_push(b"ef".to_vec()));
        outbox.finish();
        assert!(!outbox.push(b"x".to_vec(), || true), "finish 后不再收");
        let mut got = Vec::new();
        // 写线程退出时丢掉 sink，这边读到 EOF。
        far.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"abcdef");
        ended.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn full_queue_refuses_and_push_gives_up_when_told() {
        let (outbox, _far, _ended) = start();
        fill(&outbox);
        assert!(!outbox.try_push(b"reply".to_vec()), "排满时 try_push 不收");
        assert!(outbox.force_push(b"must".to_vec()), "force_push 不看上限");
        let gone = AtomicBool::new(false);
        let t = Instant::now();
        thread::scope(|s| {
            s.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                gone.store(true, Ordering::Relaxed);
            });
            assert!(!outbox.push(vec![b'z'; 1024], || !gone.load(Ordering::Relaxed)));
        });
        assert!(t.elapsed() < Duration::from_secs(2), "说了不等还在等");
    }

    #[test]
    fn close_drops_pending_and_ends_writer_once_sink_fails() {
        let (outbox, far, ended) = start();
        fill(&outbox);
        outbox.close();
        assert!(!outbox.force_push(b"x".to_vec()));
        // 写线程卡在写 socket 上：对面关掉后写失败，线程收尾。
        drop(far);
        ended
            .recv_timeout(Duration::from_secs(2))
            .expect("写线程没退出");
    }
}
