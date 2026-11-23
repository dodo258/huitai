//! 撞额度后到重置时间自动继续：对话菜单里的开关（`A`），和到点该不该发（[`App::auto_continue_due`]）。
//! 认撞上限读的是厂商写下的记录（「此刻」里的 [`Moment::limit`]），不读屏幕。

use super::*;
use crate::ui::keys::MenuItem;

impl App {
    /// 右键菜单：这一项固定能做的操作；只剩备份的 Claude 对话最前面加「放回 Claude 目录再接着聊」，
    /// 选中的对话那家认得出撞额度时再加「撞额度后自动继续」开关。
    pub fn menu_items(&self, sel: Selected) -> Vec<MenuItem> {
        let mut items = keys::menu(sel);
        if sel == Selected::Stopped
            && let Some(e) = self.selected_entry()
            && Self::backup_only(e)
        {
            items.insert(0, keys::put_back_item());
        }
        if matches!(sel, Selected::Running | Selected::Stopped)
            && let Some(e) = self.selected_entry()
            && e.session.vendor.info().limit_notice
        {
            items.push(keys::auto_continue_item(e.note.auto_continue));
        }
        items
    }

    /// `A`：开 / 关选中对话的自动继续。
    pub(super) fn toggle_auto_continue(&mut self) -> Vec<Effect> {
        let Some(e) = self.selected_entry() else {
            return Vec::new();
        };
        let v = e.session.vendor;
        if !v.info().limit_notice {
            self.status = Some(format!("认不出 {} 撞额度，不能自动继续", v.name()));
            return Vec::new();
        }
        let on = !e.note.auto_continue;
        let effect = Effect::Annotate {
            vendor: v,
            id: e.session.id.clone(),
            edit: Edit::AutoContinue(on),
        };
        self.status = Some(
            if on {
                "撞额度后到重置时间自动发「继续」（每次上限只发一次，会话台要开着）"
            } else {
                "关了自动继续"
            }
            .to_owned(),
        );
        vec![effect]
    }

    /// 到点该自动继续的对话：开了开关；不在跑（等你回复、刚做完，或者已经不托管、也没在别处开着）；
    /// 最后一条是撞上限、读得出重置时间，`now` 已过可继续时刻（重置后 1 分钟）；这一次上限还没继续过。
    /// 发出去的记下来，标注写回来之前不再发。
    pub fn auto_continue_due(&mut self, now: SystemTime) -> Vec<Effect> {
        let mut due = Vec::new();
        for e in self.lists.iter().flat_map(|l| &l.entries) {
            if !e.note.auto_continue {
                continue;
            }
            let Some(at) = self
                .moments
                .get(&e.session.id)
                .and_then(|m| m.limit)
                .and_then(|h| h.resume_at)
            else {
                continue;
            };
            let key = (e.session.id.clone(), unix_secs(at));
            if now < at
                || e.note.continued == Some(key.1)
                || self.auto_sent.contains(&key)
                || !self.stopped_for_input(e)
            {
                continue;
            }
            due.push((key, e.clone()));
        }
        due.into_iter()
            .map(|(key, e)| {
                let hosted = self.hosted_id(&e.session.id).clone();
                let resume_at = key.1;
                self.auto_sent.insert(key);
                Effect::AutoContinue {
                    entry: Box::new(e),
                    hosted,
                    resume_at,
                }
            })
            .collect()
    }

    /// CLI 停着等输入：托管着的是等你回复、刚做完；没托管的（闲置收回了、自己退了）不能是在别处开着。
    fn stopped_for_input(&self, e: &Entry) -> bool {
        match self.state_of(&e.session.id) {
            SessionState::Running => false,
            SessionState::Waiting | SessionState::Done => true,
            SessionState::Stopped => !self.attached.contains(&e.session.id) && !self.elsewhere(e),
        }
    }

    /// 开着自动继续、那家认得出撞额度的对话：停着的也要读「此刻」，才知道它最后是不是撞了上限。
    pub(super) fn armed(e: &Entry) -> bool {
        e.note.auto_continue && e.session.vendor.info().limit_notice
    }
}

fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
