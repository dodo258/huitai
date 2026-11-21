//! 会话台自己的标注：改名、置顶、归档写进数据目录的 annotations.json，重开还在；从不写厂商文件。

mod index_common;

use std::fs;

use huitai::core::{SessionId, Vendor};
use huitai::index::{Edit, Note, ProjectEdit, ProjectNote};
use index_common::Env;

fn id(s: &str) -> SessionId {
    SessionId::from(s)
}

#[test]
fn notes_persist_across_reopen_and_never_touch_vendor_files() {
    let env = Env::new("persist");
    env.write_session("a", "修购物车", "好");
    env.write_session("b", "加优惠券", "好");
    let vendor_file = env.session_path("a");
    let before = (
        fs::read(&vendor_file).unwrap(),
        fs::metadata(&vendor_file).unwrap().modified().unwrap(),
    );

    {
        let index = env.open();
        index.refresh();
        index
            .annotate(
                Vendor::Claude,
                &id("a"),
                Edit::Rename(Some(" 结算按钮 ".into())),
            )
            .unwrap();
        index
            .annotate(Vendor::Claude, &id("a"), Edit::Pin(true))
            .unwrap();
        index
            .annotate(Vendor::Claude, &id("b"), Edit::Archive(true))
            .unwrap();
    }

    let index = env.open();
    let list = index.list(Vendor::Claude).unwrap();
    let a = list.iter().find(|e| e.session.id.as_str() == "a").unwrap();
    assert_eq!(a.title(), "结算按钮");
    assert_eq!(a.session.title.as_deref(), Some("修购物车"), "厂商标题不动");
    assert!(a.note.pinned);
    let b = list.iter().find(|e| e.session.id.as_str() == "b").unwrap();
    assert!(b.note.archived && !b.note.pinned);

    assert!(env.data().join("annotations.json").is_file());
    let after = (
        fs::read(&vendor_file).unwrap(),
        fs::metadata(&vendor_file).unwrap().modified().unwrap(),
    );
    assert_eq!(before, after, "厂商会话文件一个字节都没变");
    // 厂商目录里没有多出文件。
    let files: Vec<_> = fs::read_dir(vendor_file.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(files.len(), 2);
}

#[test]
fn pinned_sort_first_and_clearing_removes_the_record() {
    let env = Env::new("pin");
    env.write_session("old", "旧", "好");
    env.write_session("new", "新", "好");
    env.append(
        "new",
        &index_common::assistant_line("再说一句", "2026-11-03T09:00:00.000Z"),
    );
    let index = env.open();
    index.refresh();
    assert_eq!(
        index.list(Vendor::Claude).unwrap()[0].session.id.as_str(),
        "new"
    );

    index
        .annotate(Vendor::Claude, &id("old"), Edit::Pin(true))
        .unwrap();
    assert_eq!(
        index.list(Vendor::Claude).unwrap()[0].session.id.as_str(),
        "old"
    );

    index
        .annotate(Vendor::Claude, &id("old"), Edit::Pin(false))
        .unwrap();
    index
        .annotate(Vendor::Claude, &id("old"), Edit::Rename(Some("x".into())))
        .unwrap();
    index
        .annotate(Vendor::Claude, &id("old"), Edit::Rename(None))
        .unwrap();
    let list = index.list(Vendor::Claude).unwrap();
    assert_eq!(list[0].session.id.as_str(), "new");
    assert_eq!(list[1].note, Note::default());
    assert_eq!(list[1].title(), "旧");
    let json = fs::read_to_string(env.data().join("annotations.json")).unwrap();
    assert!(
        !json.contains("\"old\""),
        "全是默认值的标注不留记录：{json}"
    );
}

#[test]
fn notes_are_per_vendor() {
    let env = Env::new("vendor");
    env.write_session("same", "一", "好");
    let index = env.open();
    index.refresh();
    index
        .annotate(Vendor::Codex, &id("same"), Edit::Pin(true))
        .unwrap();
    assert!(!index.list(Vendor::Claude).unwrap()[0].note.pinned);
}

#[test]
fn corrupt_notes_file_is_set_aside_not_lost() {
    let env = Env::new("corrupt");
    env.write_session("a", "一", "好");
    fs::create_dir_all(env.data()).unwrap();
    fs::write(env.data().join("annotations.json"), "{坏了").unwrap();

    let index = env.open();
    assert_eq!(index.problems().len(), 1, "{:?}", index.problems());
    let kept: Vec<_> = fs::read_dir(env.data())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("annotations.json.corrupt-"))
        .collect();
    assert_eq!(kept.len(), 1);
    assert_eq!(
        fs::read_to_string(env.data().join(&kept[0])).unwrap(),
        "{坏了"
    );

    index.refresh();
    index
        .annotate(Vendor::Claude, &id("a"), Edit::Pin(true))
        .unwrap();
    assert!(index.list(Vendor::Claude).unwrap()[0].note.pinned);
}

#[test]
fn another_process_edit_is_not_overwritten() {
    let env = Env::new("two");
    env.write_session("a", "一", "好");
    env.write_session("b", "二", "好");
    let first = env.open();
    let second = env.open();
    first.refresh();
    first
        .annotate(Vendor::Claude, &id("a"), Edit::Pin(true))
        .unwrap();
    second
        .annotate(Vendor::Claude, &id("b"), Edit::Archive(true))
        .unwrap();

    let list = env.open().list(Vendor::Claude).unwrap();
    assert!(
        list.iter()
            .any(|e| e.session.id.as_str() == "a" && e.note.pinned)
    );
    assert!(
        list.iter()
            .any(|e| e.session.id.as_str() == "b" && e.note.archived)
    );
}

#[test]
fn expanded_projects_persist_and_merge_with_other_process() {
    let env = Env::new("expanded");
    env.write_session("a", "一", "好");
    let shop = std::path::Path::new("/Users/demo/projects/shop");
    let blog = std::path::Path::new("/Users/demo/projects/blog");
    let first = env.open();
    let second = env.open();
    assert!(first.expanded_projects().is_empty(), "项目组默认收起");

    first.set_expanded(shop, true).unwrap();
    second.set_expanded(blog, true).unwrap();
    // 和改名、置顶存在同一个文件里，互不覆盖。
    second
        .annotate(Vendor::Claude, &id("a"), Edit::Pin(true))
        .unwrap();

    let reopened = env.open();
    let got = reopened.expanded_projects();
    assert!(got.contains(shop) && got.contains(blog), "{got:?}");

    reopened.set_expanded(shop, false).unwrap();
    let got = env.open().expanded_projects();
    assert!(!got.contains(shop) && got.contains(blog), "{got:?}");
    reopened.refresh();
    assert!(reopened.list(Vendor::Claude).unwrap()[0].note.pinned);
}

#[test]
fn old_notes_file_without_expanded_still_loads() {
    let env = Env::new("old-format");
    fs::create_dir_all(env.data()).unwrap();
    fs::write(
        env.data().join("annotations.json"),
        r#"{"version":1,"notes":[{"vendor":"claude","id":"a","pinned":true}]}"#,
    )
    .unwrap();
    let index = env.open();
    assert!(index.problems().is_empty(), "{:?}", index.problems());
    assert!(index.expanded_projects().is_empty());
}

#[test]
fn project_name_and_archive_persist_without_touching_sessions_or_folder() {
    let env = Env::new("project");
    env.write_session("a", "修购物车", "好");
    let shop = std::path::Path::new("/Users/demo/projects/shop");
    let blog = std::path::Path::new("/Users/demo/projects/blog");
    let first = env.open();
    let second = env.open();
    first.refresh();
    assert!(first.projects().is_empty());

    first
        .annotate_project(shop, ProjectEdit::Rename(Some("  商城 ".into())))
        .unwrap();
    // 另一个进程同时归档别的项目、改对话标注，互不覆盖。
    second
        .annotate_project(blog, ProjectEdit::Archive(true))
        .unwrap();
    second
        .annotate(Vendor::Claude, &id("a"), Edit::Pin(true))
        .unwrap();

    let reopened = env.open();
    let got = reopened.projects();
    assert_eq!(got[shop].name.as_deref(), Some("商城"), "去掉首尾空白");
    assert!(!got[shop].archived);
    assert!(got[blog].archived && got[blog].name.is_none());
    reopened.refresh();
    let a = &reopened.list(Vendor::Claude).unwrap()[0];
    assert!(a.note.pinned && !a.note.archived, "整组归档不改对话的标注");

    // 清空名字恢复原名，取消归档；全是默认值就不留记录。
    reopened
        .annotate_project(shop, ProjectEdit::Rename(Some("   ".into())))
        .unwrap();
    reopened
        .annotate_project(blog, ProjectEdit::Archive(false))
        .unwrap();
    assert_eq!(env.open().projects().get(shop), None::<&ProjectNote>);
    let json = fs::read_to_string(env.data().join("annotations.json")).unwrap();
    assert!(!json.contains("projects"), "{json}");
}
