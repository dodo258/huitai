//! 会话台自己的标注：改名、置顶、归档写进数据目录的 annotations.json，重开还在；从不写厂商文件。

mod index_common;

use std::fs;
use std::path::Path;

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

/// 0.1.3 读 `annotations.json` 的格式（照它的源码）：厂商只认这 8 家，有一条不认得整份解析失败。
mod v013 {
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Vendor {
        Claude,
        Codex,
        Pi,
        Hermes,
        Kimi,
        Gemini,
        OpenCode,
        Qwen,
    }

    #[derive(Deserialize)]
    pub struct Record {
        /// 只为让不认得的厂商解析失败，值不用。
        #[serde(rename = "vendor")]
        pub _vendor: Vendor,
        pub id: String,
    }

    #[derive(Deserialize)]
    pub struct FileFormat {
        pub notes: Vec<Record>,
    }
}

/// 0.1.3 能不能读这份标注文件，读得了返回里面的 id。
fn read_as_v013(path: &Path) -> Result<Vec<String>, serde_json::Error> {
    let text = fs::read_to_string(path).unwrap();
    let file: v013::FileFormat = serde_json::from_str(&text)?;
    Ok(file.notes.into_iter().map(|r| r.id).collect())
}

fn ids_in(path: &Path) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_owned())
        .collect()
}

/// 审计 v0.1.4 第 9 条（标注）：0.1.3 之后加的厂商（Antigravity）的标注不写进 `annotations.json`，
/// 0.1.3 打开时不会把整份当坏文件改名、连 Claude 的标注一起丢；新版重开两边都在。
#[test]
fn new_vendor_notes_stay_out_of_the_file_old_versions_read() {
    let env = Env::new("notes-v013");
    {
        let index = env.open();
        index
            .annotate(Vendor::Antigravity, &id("agy-1"), Edit::Pin(true))
            .unwrap();
        index
            .annotate(Vendor::Claude, &id("a"), Edit::Rename(Some("结算".into())))
            .unwrap();
    }
    let main = env.data().join("annotations.json");
    assert_eq!(read_as_v013(&main).expect("0.1.3 读得了"), ["a"]);
    assert_eq!(ids_in(&env.data().join("annotations-more.json")), ["agy-1"]);

    // 新版重开再改一条：两份都还在。
    let index = env.open();
    assert!(index.problems().is_empty(), "{:?}", index.problems());
    index
        .annotate(Vendor::Codex, &id("c"), Edit::Archive(true))
        .unwrap();
    let mut old = read_as_v013(&main).unwrap();
    old.sort();
    assert_eq!(old, ["a", "c"]);
    assert_eq!(ids_in(&env.data().join("annotations-more.json")), ["agy-1"]);
}

/// 同一条：分文件之前的新版已经把 Antigravity 的标注写进了 `annotations.json`，打开就分开写回；
/// 以后的版本加的厂商（这里的 `futurecli`）解不了也原样留着，不把整份当坏文件。
#[test]
fn notes_written_before_the_split_are_moved_on_open() {
    let env = Env::new("notes-split");
    fs::create_dir_all(env.data()).unwrap();
    let main = env.data().join("annotations.json");
    fs::write(
        &main,
        r#"{"version":1,"notes":[
            {"vendor":"claude","id":"a","pinned":true},
            {"vendor":"antigravity","id":"agy-1","archived":true},
            {"vendor":"futurecli","id":"f-1","pinned":true}
        ],"projects":{"/p/shop":{"name":"商店"}}}"#,
    )
    .unwrap();
    let index = env.open();
    assert!(index.problems().is_empty(), "{:?}", index.problems());
    assert_eq!(read_as_v013(&main).expect("打开后 0.1.3 读得了"), ["a"]);
    let mut more = ids_in(&env.data().join("annotations-more.json"));
    more.sort();
    assert_eq!(more, ["agy-1", "f-1"]);
    assert_eq!(
        index
            .projects()
            .get(Path::new("/p/shop"))
            .and_then(|n| n.name.clone()),
        Some("商店".to_owned())
    );
    // 再改一条，不认得的那条照旧留着。
    index
        .annotate(Vendor::Antigravity, &id("agy-2"), Edit::Pin(true))
        .unwrap();
    let mut more = ids_in(&env.data().join("annotations-more.json"));
    more.sort();
    assert_eq!(more, ["agy-1", "agy-2", "f-1"]);
}
