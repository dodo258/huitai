//! Claude 对话 30 天自动删除的提醒和备份：`cleanupPeriodDays` 解析，备份、更新、跟着进回收站，
//! 原文件被删以后从备份列出来（预览、整段、搜索、导出），放回原处不覆盖。会话文件都是手写的脱敏假数据。

mod index_common;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use huitai::core::{Account, Vendor};
use huitai::export;
use huitai::index::{Edit, Index};
use huitai::sources::backup::{self, BACKUP_DIR, Backups, Saved};
use huitai::sources::claude::{self, RetentionFrom};
use huitai::sources::{ClaudeSource, Source, Trash, sources_for};

use index_common::{Env, PROJECT, assistant_line, temp_dir, user_line};

// ---- cleanupPeriodDays ----

fn retention_of(settings: Option<&str>) -> claude::Retention {
    // 并行跑的测试各用各的目录。
    static N: AtomicUsize = AtomicUsize::new(0);
    let d = temp_dir(&format!("retention-{}", N.fetch_add(1, Ordering::Relaxed)));
    if let Some(s) = settings {
        fs::write(d.join("settings.json"), s).unwrap();
    }
    claude::retention(&d)
}

#[test]
fn retention_unset_is_the_official_default() {
    let r = retention_of(None);
    assert_eq!((r.days, r.from), (30, RetentionFrom::Default));
    let r = retention_of(Some(r#"{"model":"opus"}"#));
    assert_eq!((r.days, r.from), (30, RetentionFrom::Default));
}

#[test]
fn retention_set_is_used() {
    let r = retention_of(Some(r#"{"cleanupPeriodDays": 7}"#));
    assert_eq!((r.days, r.from), (7, RetentionFrom::Set));
    // JSON 里 365.0 也是整数。
    let r = retention_of(Some(r#"{"cleanupPeriodDays":365.0,"x":1}"#));
    assert_eq!((r.days, r.from), (365, RetentionFrom::Set));
}

#[test]
fn retention_invalid_values_fall_back_to_default_and_say_so() {
    // 官方：不小于 1 的整数，0 通不过校验。
    for raw in ["0", "-3", "1.5", "\"30\"", "null", "true"] {
        let r = retention_of(Some(&format!(r#"{{"cleanupPeriodDays": {raw}}}"#)));
        assert_eq!(r.days, 30, "{raw}");
        assert!(
            matches!(&r.from, RetentionFrom::Invalid(v) if v == raw),
            "{raw}: {:?}",
            r.from
        );
    }
    let r = retention_of(Some("{ not json"));
    assert_eq!(r.days, 30);
    assert!(matches!(r.from, RetentionFrom::Invalid(_)));
}

#[test]
fn deleted_in_counts_from_last_activity() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100 * 86_400);
    let day = Duration::from_secs(86_400);
    assert_eq!(claude::deleted_in(now - day * 28, now, 30), Some(day * 2));
    assert_eq!(claude::deleted_in(now - day * 31, now, 30), None);
}

// ---- 备份、更新、删除 ----

/// 测试环境：`Env` 的 Claude 项目目录，加上会话台数据目录下的备份目录。
struct Kit {
    env: Env,
}

impl Kit {
    fn new(name: &str) -> Self {
        Self {
            env: Env::new(name),
        }
    }

    fn backups(&self) -> Backups {
        Backups::for_account(&self.env.data(), "")
    }

    fn source(&self) -> ClaudeSource {
        ClaudeSource::new(
            self.env.projects(),
            Trash::new(self.env.data().join("trash")),
        )
        .with_backups(self.backups())
    }

    fn open(&self) -> Index {
        let index = Index::open(&self.env.data(), vec![Box::new(self.source())]).unwrap();
        index.refresh();
        index
    }

    fn backup_path(&self, id: &str) -> PathBuf {
        self.env
            .data()
            .join(BACKUP_DIR)
            .join(PROJECT)
            .join(format!("{id}.jsonl.zst"))
    }

    fn write(&self, id: &str, user: &str, answer: &str) {
        self.env.write_session(id, user, answer);
    }
}

/// 一轮备份（只备标注过的，默认 30 天）。
fn back_up(index: &Index, all: bool) -> huitai::index::BackupReport {
    index.back_up_claude(all, &HashMap::new(), SystemTime::now(), &mut HashSet::new())
}

fn unzstd(path: &Path) -> Vec<u8> {
    zstd::stream::decode_all(fs::File::open(path).unwrap()).unwrap()
}

#[test]
fn annotated_sessions_are_backed_up_and_updated_when_the_original_changes() {
    let k = Kit::new("backup-annotated");
    k.write("s-pinned", "给结算页加骨架屏", "好，先看组件结构。");
    k.write("s-plain", "随便问问", "嗯。");
    let index = k.open();

    // 没标注的不备。
    let r = back_up(&index, false);
    assert_eq!((r.written, r.fresh), (0, 0), "{:?}", r.problems);
    assert!(!k.env.data().join(BACKUP_DIR).exists());

    index
        .annotate(Vendor::Claude, &"s-pinned".into(), Edit::Pin(true))
        .unwrap();
    let r = back_up(&index, false);
    assert_eq!((r.written, r.fresh), (1, 0), "{:?}", r.problems);
    let bk = k.backup_path("s-pinned");
    assert_eq!(
        unzstd(&bk),
        fs::read(k.env.session_path("s-pinned")).unwrap()
    );
    assert!(!k.backup_path("s-plain").exists());
    // 备份目录 0700、文件 0600。
    for d in [k.env.data().join("backups"), k.env.data().join(BACKUP_DIR)] {
        let mode = fs::metadata(&d).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", d.display());
    }
    assert_eq!(
        fs::metadata(&bk).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // 原文件没变：不重写。
    let before = fs::metadata(&bk).unwrap().modified().unwrap();
    assert_eq!(back_up(&index, false).fresh, 1);

    // 原文件变了：更新。
    k.env.append(
        "s-pinned",
        &user_line("s-pinned", "再加个加载动画", "2026-11-01T10:00:00.000Z"),
    );
    let later = SystemTime::now() + Duration::from_secs(5);
    fs::File::options()
        .append(true)
        .open(k.env.session_path("s-pinned"))
        .unwrap()
        .set_modified(later)
        .unwrap();
    assert_eq!(back_up(&index, false).written, 1);
    assert_eq!(
        unzstd(&bk),
        fs::read(k.env.session_path("s-pinned")).unwrap()
    );
    assert_ne!(fs::metadata(&bk).unwrap().modified().unwrap(), before);

    // 改名、归档的也备；设置里开了「备份全部」就全备。
    index
        .annotate(
            Vendor::Claude,
            &"s-plain".into(),
            Edit::Rename(Some("问问".into())),
        )
        .unwrap();
    assert_eq!(back_up(&index, false).written, 1);
    assert!(k.backup_path("s-plain").exists());
    k.write("s-new", "新的一条", "好。");
    index.refresh();
    assert_eq!(back_up(&index, false).written, 0);
    assert_eq!(back_up(&index, true).written, 1);
    assert!(k.backup_path("s-new").exists());
}

#[test]
fn near_deletion_check_rewrites_a_broken_backup() {
    let k = Kit::new("backup-check");
    k.write("s1", "问题", "回答");
    let src = k.source();
    let index = k.open();
    let s = index.list(Vendor::Claude).unwrap().remove(0).session;
    assert_eq!(src.back_up(&s, false).unwrap(), Saved::Written);
    // 备份坏了（截断），修改时间还和原文件一样。
    let bk = k.backup_path("s1");
    let mtime = fs::metadata(&bk).unwrap().modified().unwrap();
    let f = fs::File::options().write(true).open(&bk).unwrap();
    f.set_len(5).unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    assert_eq!(
        src.back_up(&s, false).unwrap(),
        Saved::Fresh,
        "不确认就不知道坏了"
    );
    assert_eq!(src.back_up(&s, true).unwrap(), Saved::Written);
    backup::verify(&bk).unwrap();
}

/// 校验没过、重写又失败：旧备份留着（校验没过可能只是一时读不了），能重写了再换上。
#[test]
fn near_deletion_check_keeps_the_old_backup_when_rewriting_fails() {
    let k = Kit::new("backup-check-fails");
    k.write("s1", "问题", "回答");
    let src = k.source();
    let index = k.open();
    let s = index.list(Vendor::Claude).unwrap().remove(0).session;
    src.back_up(&s, false).unwrap();
    let original = k.env.session_path("s1");
    let text = fs::read(&original).unwrap();
    let bk = k.backup_path("s1");
    let chmod = |p: &Path, mode| fs::set_permissions(p, fs::Permissions::from_mode(mode)).unwrap();

    // 备份一时读不了（内容是好的），原文件也读不了。
    chmod(&bk, 0o000);
    chmod(&original, 0o000);
    let failed = src.back_up(&s, true);
    chmod(&original, 0o600);
    assert!(failed.is_err(), "{failed:?}");
    assert!(bk.exists(), "重写失败不能连旧备份也没了");
    chmod(&bk, 0o600);
    assert_eq!(unzstd(&bk), text);
    let dir = bk.parent().unwrap();
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1, "临时文件没清");

    // 备份真坏了、原文件又读不了：坏的也先留着，原文件读得了了再换成好的。
    let mtime = fs::metadata(&bk).unwrap().modified().unwrap();
    let f = fs::File::options().write(true).open(&bk).unwrap();
    f.set_len(5).unwrap();
    f.set_modified(mtime).unwrap();
    drop(f);
    chmod(&original, 0o000);
    let failed = src.back_up(&s, true);
    chmod(&original, 0o600);
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(fs::metadata(&bk).unwrap().len(), 5);
    assert_eq!(src.back_up(&s, true).unwrap(), Saved::Written);
    assert_eq!(unzstd(&bk), text);
    assert_eq!(fs::read_dir(dir).unwrap().count(), 1);
}

#[test]
fn trash_takes_the_backup_along_and_restore_brings_it_back() {
    let k = Kit::new("backup-trash");
    k.write("s1", "问题", "回答");
    let src = k.source();
    let index = k.open();
    let s = index.list(Vendor::Claude).unwrap().remove(0).session;
    src.back_up(&s, false).unwrap();
    let bk = k.backup_path("s1");

    let entry = src.trash(&s).unwrap();
    assert!(!s.path.exists() && !bk.exists());
    src.restore(&entry).unwrap();
    assert!(s.path.exists() && bk.exists());
}

// ---- 原文件没了以后 ----

/// 备好一条、再把原文件删掉（模拟 Claude 清理），返回 index 和备份路径。
fn deleted_by_claude(k: &Kit) -> (Index, PathBuf) {
    k.write(
        "s-old",
        "怎么给订单表加索引",
        "先看慢查询日志，再按 created_at 建联合索引。",
    );
    k.env.append(
        "s-old",
        &assistant_line(
            "索引建好了，查询从 900ms 降到 12ms。",
            "2026-11-01T09:01:00.000Z",
        ),
    );
    let index = k.open();
    index
        .annotate(Vendor::Claude, &"s-old".into(), Edit::Archive(true))
        .unwrap();
    assert_eq!(back_up(&index, false).written, 1);
    let original = k.env.session_path("s-old");
    fs::remove_file(&original).unwrap();
    (index, k.backup_path("s-old"))
}

#[test]
fn backup_stands_in_when_claude_deletes_the_original() {
    let k = Kit::new("backup-standin");
    let (index, bk) = deleted_by_claude(&k);
    // FSEvents 只报来原文件没了：照样列着，换成备份。
    index.refresh_paths(Vendor::Claude, &[k.env.session_path("s-old")]);
    let list = index.list(Vendor::Claude).unwrap();
    assert_eq!(list.len(), 1);
    let e = &list[0];
    assert_eq!(e.session.id.as_str(), "s-old");
    assert_eq!(e.session.path, bk);
    assert_eq!(e.session.cwd, Path::new("/Users/demo/projects/shop"));
    assert_eq!(e.session.message_count, 1);
    assert_eq!(e.title(), "怎么给订单表加索引");
    assert!(e.note.archived, "标注按 id 跟着走");

    // 预览、整段、精确计数都读得到，读完不留临时文件。
    let src = k.source();
    let preview = src.load_preview(&e.session, 8).unwrap();
    assert_eq!(preview.len(), 3);
    assert!(preview[2].text.contains("12ms"));
    let tr = src.load_transcript(&e.session).unwrap();
    assert!(!tr.parts.is_empty());
    assert_eq!(src.count_messages(&e.session).unwrap(), 1);
    let open_dir = k.env.data().join(BACKUP_DIR).join(".open");
    assert_eq!(fs::read_dir(&open_dir).unwrap().count(), 0);

    // 搜正文搜得到。
    index.index_bodies(&mut || true).unwrap();
    let hits = index.search(Vendor::Claude, "慢查询日志", 10).unwrap();
    assert_eq!(hits.len(), 1);

    // 导出有正文。
    let md = export::render(&src, e, &[], "shop").unwrap();
    assert!(md.contains("900ms 降到 12ms"), "{md}");

    // 整家重扫也一样（启动时）。
    let again = k.open();
    assert_eq!(again.list(Vendor::Claude).unwrap()[0].session.path, bk);
    // Claude 目录里什么也没多。
    let dir = k.env.projects().join(PROJECT);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn put_back_restores_the_original_and_never_overwrites() {
    let k = Kit::new("backup-putback");
    let (index, bk) = deleted_by_claude(&k);
    index.refresh_paths(Vendor::Claude, &[k.env.session_path("s-old")]);
    let e = index.list(Vendor::Claude).unwrap().remove(0);
    let src = k.source();
    let original = k.env.session_path("s-old");

    // 原处被占：不覆盖，原处的内容不变。
    fs::write(&original, b"someone else").unwrap();
    let err = src.put_back(&e.session).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists, "{err}");
    assert_eq!(fs::read(&original).unwrap(), b"someone else");
    fs::remove_file(&original).unwrap();

    // 放回：内容和备份一样，修改时间是现在（不然下次开 Claude 又删了），备份留着。
    assert_eq!(src.put_back(&e.session).unwrap(), original);
    assert_eq!(fs::read(&original).unwrap(), unzstd(&bk));
    let age = SystemTime::now()
        .duration_since(fs::metadata(&original).unwrap().modified().unwrap())
        .unwrap();
    assert!(age < Duration::from_secs(60));
    assert!(bk.exists());
    // 列表换回原文件。
    index.refresh_paths(Vendor::Claude, std::slice::from_ref(&original));
    let list = index.list(Vendor::Claude).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].session.path, original);
    // 原文件在的对话没什么可放回。
    assert!(src.put_back(&list[0].session).is_err());
}

#[test]
fn backups_live_next_to_the_trash_per_account() {
    let home = temp_dir("backup-accounts");
    let data = home.join("data");
    let work = home.join(".claude-work");
    let accounts = [
        Account::new(Vendor::Claude, "默认", home.join(".claude"), &home),
        Account::new(Vendor::Claude, "工作号", work.clone(), &home),
    ];
    for (a, sub) in accounts.iter().zip(["", accounts[1].key.as_str()]) {
        let projects = a.dir.join("projects").join(PROJECT);
        fs::create_dir_all(&projects).unwrap();
        fs::write(
            projects.join("s.jsonl"),
            format!("{}\n", user_line("s", "你好", "2026-11-01T09:00:00.000Z")),
        )
        .unwrap();
        let srcs = sources_for(std::slice::from_ref(a), Trash::new(data.join("trash")));
        let src = &srcs[0];
        let s = src.scan().sessions.remove(0);
        assert_eq!(src.back_up(&s, false).unwrap(), Saved::Written);
        let want = data
            .join(BACKUP_DIR)
            .join(sub)
            .join(PROJECT)
            .join("s.jsonl.zst");
        assert!(want.is_file(), "{}", want.display());
    }
    let (n, bytes) = backup::usage(&data.join(BACKUP_DIR));
    assert_eq!(n, 2);
    assert!(bytes > 0);
    // 默认账号列不出工作号的备份。
    let srcs = sources_for(&accounts[..1], Trash::new(data.join("trash")));
    fs::remove_file(home.join(".claude/projects").join(PROJECT).join("s.jsonl")).unwrap();
    let ids: Vec<String> = srcs[0]
        .scan()
        .sessions
        .iter()
        .map(|s| s.id.to_string())
        .collect();
    assert_eq!(ids, ["s"]);
}
