//! 多账号：同一家几个数据目录（两个假 `CLAUDE_CONFIG_DIR`、两个假 `CODEX_HOME`）一起列出，按 canonical 路径去重，
//! 恢复命令带对的目录变量；旧缓存升级不丢改名、置顶。数据全是 tests/fixtures 里手写的脱敏样例。

mod sources_common;

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;

use huitai::core::{Account, AccountSetting, Session, Vendor, VendorDirs};
use huitai::index::{Edit, Index};
use huitai::sources::{Trash, account_env_prefix, accounts, source_of, sources_for};
use sources_common::{copy_tree, fixture, temp_dir};

/// 假家目录：`claude-a`（环境变量指的）、`claude-b`（设置里加的）各放一份 Claude 样例，
/// `claude-link` 的 `projects` 链回 `claude-a/projects`（cc-switch 这类工具的做法）；
/// `codex-a`（环境变量）、`codex-b`（设置）各放一份 Codex rollout 样例（同样的 id）。
struct Home {
    home: PathBuf,
}

impl Home {
    fn new(name: &str) -> Self {
        let home = temp_dir(name);
        for d in ["claude-a", "claude-b"] {
            copy_tree(&fixture("claude/projects"), &home.join(d).join("projects"));
        }
        fs::create_dir_all(home.join("claude-link")).unwrap();
        std::os::unix::fs::symlink(
            home.join("claude-a/projects"),
            home.join("claude-link/projects"),
        )
        .unwrap();
        for d in ["codex-a", "codex-b"] {
            copy_tree(&fixture("codex/sessions"), &home.join(d).join("sessions"));
            fs::copy(
                fixture("codex/session_index.jsonl"),
                home.join(d).join("session_index.jsonl"),
            )
            .unwrap();
        }
        Self { home }
    }

    fn env(&self) -> impl Fn(&str) -> Option<OsString> + '_ {
        move |k| match k {
            "CLAUDE_CONFIG_DIR" => Some(self.home.join("claude-a").into()),
            "CODEX_HOME" => Some(self.home.join("codex-a").into()),
            _ => None,
        }
    }

    fn settings(&self) -> Vec<AccountSetting> {
        let s = |vendor, name: &str, dir: &str| AccountSetting {
            vendor,
            name: name.to_owned(),
            dir: dir.to_owned(),
        };
        vec![
            s(Vendor::Claude, "乙", "claude-b"),
            s(Vendor::Claude, "链过去的", "claude-link"),
            // 和环境变量指的是同一个目录（多了末尾的 `/`）：只列一次，用这里起的名字。
            s(Vendor::Codex, "甲", "codex-a/"),
            s(Vendor::Codex, "乙", "codex-b"),
        ]
    }

    fn accounts(&self) -> Vec<Account> {
        accounts(&self.home, &self.home, self.env(), &self.settings())
    }

    fn trash(&self) -> Trash {
        Trash::new(self.home.join("trash"))
    }
}

fn of(accounts: &[Account], v: Vendor) -> Vec<(&str, PathBuf)> {
    accounts
        .iter()
        .filter(|a| a.vendor == v)
        .map(|a| (a.name.as_str(), a.dir.clone()))
        .collect()
}

#[test]
fn accounts_are_found_and_deduplicated_by_canonical_path() {
    let h = Home::new("acct-find");
    let list = h.accounts();
    assert_eq!(
        of(&list, Vendor::Claude),
        [
            ("claude-a", h.home.join("claude-a")),
            ("乙", h.home.join("claude-b")),
        ],
        "claude-link 的 projects 链回 claude-a，去掉"
    );
    assert_eq!(
        of(&list, Vendor::Codex),
        [
            ("甲", h.home.join("codex-a")),
            ("乙", h.home.join("codex-b"))
        ]
    );
    // 环境变量都设了、家目录下默认位置不在：两家都没有默认账号。
    assert!(
        list.iter()
            .filter(|a| matches!(a.vendor, Vendor::Claude | Vendor::Codex))
            .all(|a| !a.is_default())
    );
}

/// 两个账号里同一个对话 id 各列一条，id 带各自账号的前缀，不撞；恢复命令带那个账号的目录变量。
#[test]
fn every_account_is_listed_and_resumes_with_its_own_dir() {
    let h = Home::new("acct-list");
    let accounts = h.accounts();
    let sources = sources_for(&accounts, h.trash());
    let index = Index::open(&h.home.join("data"), sources_for(&accounts, h.trash())).unwrap();
    let reports = index.refresh();
    assert!(reports.iter().all(|r| !r.rebuilt), "{reports:?}");

    for vendor in [Vendor::Claude, Vendor::Codex] {
        let listed = index.list(vendor).unwrap();
        let by_account = group(listed.iter().map(|e| &e.session));
        let mine: Vec<&Account> = accounts.iter().filter(|a| a.vendor == vendor).collect();
        assert_eq!(by_account.len(), 2, "{vendor} 两个账号都列出来了");
        // 两个账号的样例一样：同样的对话、同样多条。
        let natives: Vec<HashSet<String>> = mine
            .iter()
            .map(|a| {
                by_account[a.key.as_str()]
                    .iter()
                    .map(|s| s.id.native().to_owned())
                    .collect()
            })
            .collect();
        assert!(!natives[0].is_empty());
        assert_eq!(natives[0], natives[1], "{vendor}");
        let ids: HashSet<&str> = listed.iter().map(|e| e.session.id.as_str()).collect();
        assert_eq!(ids.len(), listed.len(), "{vendor} 的 id 不撞");

        for a in &mine {
            let [(var, Some(dir))] = &a.env(&h.home)[..] else {
                panic!("都不在默认位置，只带一个变量");
            };
            assert_eq!(
                account_env_prefix(a, &h.home),
                format!("{var}={} ", dir.display())
            );
            for s in &by_account[a.key.as_str()] {
                assert!(s.id.as_str().starts_with(&format!("{}:", a.key)));
                assert_eq!(s.account, a.key);
                let src = source_of(&sources, s).expect("每个账号有自己的来源");
                let cmd = src.resume_cmd(s);
                assert!(
                    cmd.args.iter().any(|x| x == s.id.native()),
                    "交给 CLI 的是它自己的 id：{cmd:?}"
                );
                assert!(!cmd.args.iter().any(|x| x == s.id.as_str()));
                // 预览也从自己账号的目录读。
                assert!(
                    s.path.starts_with(&a.dir),
                    "{} 不在 {}",
                    s.path.display(),
                    a.dir.display()
                );
                src.load_preview(s, 5).unwrap();
            }
        }
    }
    assert_eq!(
        account_env_prefix(&accounts[0], &h.home),
        format!("CLAUDE_CONFIG_DIR={} ", h.home.join("claude-a").display())
    );
}

fn group<'a>(sessions: impl Iterator<Item = &'a Session>) -> HashMap<&'a str, Vec<&'a Session>> {
    let mut out: HashMap<&str, Vec<&Session>> = HashMap::new();
    for s in sessions {
        out.entry(s.account.as_str()).or_default().push(s);
    }
    out
}

/// 只用默认目录的老用户：升级后多了个账号，原来那些对话的 id、改名、置顶都不变。
#[test]
fn upgrading_a_default_cache_keeps_renames_and_pins() {
    let h = Home::new("acct-upgrade");
    copy_tree(
        &fixture("claude/projects"),
        &h.home.join(".claude/projects"),
    );
    let data = h.home.join("data");

    // 升级前：只有默认目录一个账号（老版本就是这样扫的）。
    let old = accounts(&h.home, &h.home, |_| None, &[]);
    assert!(old.iter().all(Account::is_default));
    let index = Index::open(&data, sources_for(&old, h.trash())).unwrap();
    index.refresh();
    let before = index.list(Vendor::Claude).unwrap();
    let first = before[0].session.id.clone();
    index
        .annotate(
            Vendor::Claude,
            &first,
            Edit::Rename(Some("改过的名字".to_owned())),
        )
        .unwrap();
    index
        .annotate(Vendor::Claude, &first, Edit::Pin(true))
        .unwrap();
    drop(index);
    {
        // 变回多账号之前的表（没有 account 列）。
        let c = rusqlite::Connection::open(data.join("index.sqlite")).unwrap();
        c.execute_batch(
            "alter table sessions drop column account; alter table files drop column account;",
        )
        .unwrap();
    }

    // 升级后：默认目录 + 设置里加的 claude-b。
    let settings = [AccountSetting {
        vendor: Vendor::Claude,
        name: "乙".to_owned(),
        dir: "claude-b".to_owned(),
    }];
    let new = accounts(&h.home, &h.home, |_| None, &settings);
    let index = Index::open(&data, sources_for(&new, h.trash())).unwrap();
    // 缓存还没重扫时就是原来那些。
    let cached = index.list(Vendor::Claude).unwrap();
    assert_eq!(cached.len(), before.len(), "补列、不删库，打开就有列表");
    assert!(cached[0].note.pinned && cached[0].session.id == first);
    index.refresh();
    let after = index.list(Vendor::Claude).unwrap();
    assert_eq!(after.len(), before.len() * 2);
    let e = after
        .iter()
        .find(|e| e.session.id == first)
        .expect("默认账号的 id 不变");
    assert_eq!(e.title(), "改过的名字");
    assert!(e.note.pinned);
    assert_eq!(after[0].session.id, first, "置顶的还在最前");
    // 另一个账号里同一个对话不跟着改名置顶。
    let twin = after
        .iter()
        .find(|x| x.session.id.native() == first.as_str() && x.session.id != first)
        .unwrap();
    assert!(!twin.note.pinned);
    assert_ne!(twin.title(), "改过的名字");
}

/// 多账号之前，设了 `CODEX_HOME` 启动的会话台把改名置顶记在 `profiles/<名>/` 里、id 不带账号前缀：
/// 新版第一次用同一组环境变量打开时搬过来，对上带前缀的 id；只搬一次。
#[test]
fn notes_from_an_old_profile_cache_move_to_the_account() {
    let h = Home::new("acct-profile");
    let data = h.home.join("data");
    let dirs = VendorDirs::new(&h.home, &h.home, h.env());
    let old_dir = dirs.cache_dir(&data, &h.home);
    assert_ne!(old_dir, data);

    let accounts = h.accounts();
    let index = Index::open(&data, sources_for(&accounts, h.trash())).unwrap();
    index.refresh();
    let codex_a = &accounts
        .iter()
        .find(|a| a.vendor == Vendor::Codex)
        .unwrap()
        .key;
    let target = index
        .list(Vendor::Codex)
        .unwrap()
        .into_iter()
        .find(|e| e.session.account == *codex_a)
        .unwrap();
    let native = target.session.id.native().to_owned();

    // 老版本写的标注：一个旧的「只有一个账号」的索引在 profiles 目录里改名置顶。
    let legacy = Index::open(&old_dir, Vec::new()).unwrap();
    legacy
        .annotate(Vendor::Codex, &native.as_str().into(), Edit::Pin(true))
        .unwrap();
    drop(legacy);

    index.adopt_profile_notes(&dirs, &data, &h.home);
    assert!(index.problems().is_empty(), "{:?}", index.problems());
    let moved = index
        .list(Vendor::Codex)
        .unwrap()
        .into_iter()
        .find(|e| e.session.id == target.session.id)
        .unwrap();
    assert!(moved.note.pinned, "搬到了带账号前缀的 id 上");
    // 别的账号里同 id 的那条不跟着置顶。
    let others = index
        .list(Vendor::Codex)
        .unwrap()
        .into_iter()
        .filter(|e| e.session.id.native() == native && e.session.id != target.session.id)
        .collect::<Vec<_>>();
    assert!(others.iter().all(|e| !e.note.pinned));

    // 只搬一次：这边取消置顶后再开一次，不会又从旧文件搬回来。
    index
        .annotate(Vendor::Codex, &target.session.id, Edit::Pin(false))
        .unwrap();
    index.adopt_profile_notes(&dirs, &data, &h.home);
    let again = index
        .list(Vendor::Codex)
        .unwrap()
        .into_iter()
        .find(|e| e.session.id == target.session.id)
        .unwrap();
    assert!(!again.note.pinned);
    assert!(!old_dir.join("annotations.json").exists());
}

#[test]
fn missing_account_dir_is_listed_but_empty() {
    let h = Home::new("acct-missing");
    let settings = [AccountSetting {
        vendor: Vendor::Claude,
        name: "还没登录".to_owned(),
        dir: "/nonexistent-huitai/claude".to_owned(),
    }];
    let list = accounts(&h.home, &h.home, h.env(), &settings);
    assert_eq!(of(&list, Vendor::Claude).len(), 2);
    let index = Index::open(&h.home.join("data"), sources_for(&list, h.trash())).unwrap();
    let reports = index.refresh();
    assert!(reports.iter().all(|r| r.problems.is_empty()), "{reports:?}");
    let listed = index.list(Vendor::Claude).unwrap();
    assert!(
        listed
            .iter()
            .all(|e| e.session.path.starts_with(h.home.join("claude-a")))
    );
}

/// Hermes 的 profile（`~/.hermes/profiles/<名>/`，有 `state.db` 的）自动认成账号，名字就是 profile 名。
#[test]
fn hermes_profiles_are_discovered() {
    let h = Home::new("acct-hermes");
    for p in ["work", "play"] {
        let dir = h.home.join(".hermes/profiles").join(p);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("state.db"), b"").unwrap();
    }
    // 没有库的目录不算。
    fs::create_dir_all(h.home.join(".hermes/profiles/empty")).unwrap();
    let list = accounts(&h.home, &h.home, |_| None, &[]);
    assert_eq!(
        of(&list, Vendor::Hermes),
        [
            ("默认", h.home.join(".hermes")),
            ("play", h.home.join(".hermes/profiles/play")),
            ("work", h.home.join(".hermes/profiles/work")),
        ]
    );
    let work = list.iter().find(|a| a.name == "work").unwrap();
    assert_eq!(
        account_env_prefix(work, &h.home),
        format!(
            "HERMES_HOME={} ",
            h.home.join(".hermes/profiles/work").display()
        )
    );
}
