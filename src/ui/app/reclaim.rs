//! 答完不发提醒信号的 CLI（pi）的闲置收回：哪些托管对话要交给收回线程看厂商记录。
//! 怎么判断和请守护进程收回在 `runtime/reclaim.rs`，守护进程那边在 `host::idle`。

use super::*;

impl App {
    /// 要看记录能不能收回的对话：托管着（守护进程里的 id，认领到临时 id 下的给临时 id）、没有标签页连着。
    /// 记录答没答完、闲置够不够久由收回线程和守护进程再看；设置里关了闲置收回就是空的。
    pub fn reclaim_targets(&self) -> Vec<(SessionId, Entry)> {
        if self.config.idle_reclaim().is_none() {
            return Vec::new();
        }
        self.lists
            .iter()
            .flat_map(|l| &l.entries)
            .filter(|e| {
                let id = &e.session.id;
                self.state_of(id) != SessionState::Stopped && !self.attached.contains(id)
            })
            .map(|e| (self.hosted_id(&e.session.id).clone(), e.clone()))
            .collect()
    }
}
