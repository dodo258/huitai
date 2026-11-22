//! 项目管理：N 的目录补全和候选、只输名字建在 projects_root 下、目录不存在先确认再建；
//! 项目组上 e 起显示名（列表、预览、标签页标题前缀都用它）、a 整组归档。

mod ui_support;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use huitai::core::{Config, Vendor};
use huitai::index::{Index, ProjectEdit, ProjectNote};
use huitai::opener;
use huitai::ui::App;
use huitai::ui::app::{Effect, Prompt};
use huitai::ui::newdir::{self, DirPicker};
use huitai::ui::rows::GroupKey;
use huitai::ui::text::project_label;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ui_support::*;

fn press(app: &mut App, c: char) -> Vec<Effect> {
    app.key(key(KeyCode::Char(c)))
}

fn typed(app: &mut App, s: &str) {
    for c in s.chars() {
        press(app, c);
    }
}

fn picker(app: &App) -> &DirPicker {
    match &app.prompt {
        Some(Prompt::NewDir { picker, .. }) => picker,
        other => panic!("不是目录输入框：{other:?}"),
    }
}

/// 筛出来的候选（显示的样子）。
fn shown(app: &App) -> Vec<String> {
    let p = picker(app);
    p.matches()
        .into_iter()
        .map(|i| p.candidates[i].shown.clone())
        .collect()
}

/// 选中某个项目组的标题。
fn select_header(app: &mut App, dir: &str) {
    app.key(key(KeyCode::Home));
    for _ in 0..100 {
        if app.selected_entry().is_none()
            && app
                .selected_group()
                .is_some_and(|g| g.key == GroupKey::Project(PathBuf::from(dir)))
        {
            return;
        }
        app.key(key(KeyCode::Down));
    }
    panic!("列表里没有项目组 {dir}");
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-ui-projects-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

// ---- N：候选、筛选、补全 ----

#[test]
fn candidates_are_project_dirs_of_all_vendors_then_projects_root() {
    let mut app = demo_app(Vendor::Claude);
    assert_eq!(
        press(&mut app, 'm'),
        vec![Effect::ScanProjectsRoot(PathBuf::from(
            "/Users/demo/projects"
        ))]
    );
    let all = shown(&app);
    // 三家的项目都有（Codex 的 ledger-cli、Claude 的 notes），每个只出现一次。
    for want in [
        "~/projects/shop",
        "~/projects/blog",
        "~/projects/api",
        "~/work/api",
        "~/notes",
        "~/projects/ledger-cli",
    ] {
        assert_eq!(
            all.iter().filter(|s| *s == want).count(),
            1,
            "{want}：{all:?}"
        );
    }
    // 工作树、家目录不算项目。
    assert!(
        !all.iter().any(|s| s.contains("worktrees") || s == "~"),
        "{all:?}"
    );

    // projects_root 的子目录读到后补在后面，已有的不重复。
    app.set_root_dirs(vec![
        PathBuf::from("/Users/demo/projects/shop"),
        PathBuf::from("/Users/demo/projects/zine"),
    ]);
    let after = shown(&app);
    assert_eq!(after.len(), all.len() + 1);
    assert_eq!(after.last().map(String::as_str), Some("~/projects/zine"));
}

#[test]
fn typing_filters_candidates_name_prefix_first() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 'm');
    app.set_root_dirs(vec![PathBuf::from("/Users/demo/projects/apiary")]);
    typed(&mut app, "API");
    // 不分大小写；目录名开头对上的在前（api、api、apiary），路径里有的（没有）在后。
    assert_eq!(
        shown(&app),
        vec!["~/projects/api", "~/work/api", "~/projects/apiary"]
    );
    typed(&mut app, "a");
    assert_eq!(shown(&app), vec!["~/projects/apiary"]);
    // 按路径筛：带 / 的输入对路径。
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "work/");
    assert_eq!(shown(&app), vec!["~/work/api"]);
    // 家目录路径（/Users/demo）里的字母不算对上；输入绝对路径时按开头对。
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "demo");
    assert!(shown(&app).is_empty(), "{:?}", shown(&app));
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "/users/demo/work");
    assert_eq!(shown(&app), vec!["~/work/api"]);
    // 目录名中间有的排在目录名开头的后面。
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "l");
    let got = shown(&app);
    assert_eq!(
        got.first().map(String::as_str),
        Some("~/projects/ledger-cli")
    );
    assert!(got.contains(&"~/projects/blog".to_owned()), "{got:?}");
}

#[test]
fn tab_completes_and_arrows_pick() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 'm');
    typed(&mut app, "api");
    // Tab：没选就补成第一个。
    app.key(key(KeyCode::Tab));
    assert_eq!(picker(&app).text, "~/projects/api");
    assert_eq!(picker(&app).picked, None);

    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    typed(&mut app, "api");
    // ↓↓ 选第二个，再 ↓ 到底停住；↑ 回到第一个，再 ↑ 是不选。
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    assert_eq!(picker(&app).picked, Some(1));
    app.key(key(KeyCode::Up));
    app.key(key(KeyCode::Up));
    assert_eq!(picker(&app).picked, None);
    // 选了第二个，Tab 补成它；再打字就取消选中。
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Tab));
    assert_eq!(picker(&app).text, "~/work/api");
    app.key(key(KeyCode::Down));
    press(&mut app, 'x');
    assert_eq!(picker(&app).picked, None);
    // 没有对得上的，Tab 什么都不做。
    let before = picker(&app).text.clone();
    app.key(key(KeyCode::Tab));
    assert_eq!(picker(&app).text, before);
}

#[test]
fn enter_on_picked_candidate_uses_it() {
    let root = tmp("picked");
    let shop = root.join("shop");
    fs::create_dir_all(&shop).unwrap();
    let mut app = demo_app(Vendor::Codex);
    press(&mut app, 'm');
    app.set_root_dirs(vec![shop.clone()]);
    typed(&mut app, "sho");
    // 第一个是 ~/projects/shop（不存在），第二个是临时目录里真的 shop。
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::New {
            vendor: Vendor::Codex,
            cwd: shop
        }]
    );
    fs::remove_dir_all(&root).unwrap();
}

// ---- 名字展开成 projects_root 下的路径 ----

#[test]
fn bare_name_goes_under_projects_root() {
    let home = Path::new("/Users/demo");
    let root = newdir::projects_root("~/code", home);
    assert_eq!(root, PathBuf::from("/Users/demo/code"));
    assert_eq!(
        newdir::projects_root("dev", home),
        PathBuf::from("/Users/demo/dev")
    );
    assert_eq!(
        newdir::projects_root("/srv/p", home),
        PathBuf::from("/srv/p")
    );
    let base = Path::new("/Users/demo/projects/shop");
    let r = |t: &str| newdir::resolve(t, home, &root, base);
    assert_eq!(r("newapp"), Some(PathBuf::from("/Users/demo/code/newapp")));
    assert_eq!(
        r("  新项目 "),
        Some(PathBuf::from("/Users/demo/code/新项目"))
    );
    assert_eq!(r("~"), Some(PathBuf::from("/Users/demo")));
    assert_eq!(r("~/x/y"), Some(PathBuf::from("/Users/demo/x/y")));
    assert_eq!(r("/opt/z"), Some(PathBuf::from("/opt/z")));
    // 带 / 的相对路径和 . .. 相对启动目录，.. 按字面消掉。
    assert_eq!(
        r("sub/dir"),
        Some(PathBuf::from("/Users/demo/projects/shop/sub/dir"))
    );
    assert_eq!(
        r("../blog"),
        Some(PathBuf::from("/Users/demo/projects/blog"))
    );
    assert_eq!(r(".."), Some(PathBuf::from("/Users/demo/projects")));
    assert_eq!(r("   "), None);
}

#[test]
fn bare_name_uses_configured_projects_root_and_shows_target() {
    let mut app = App::new(
        PathBuf::from(HOME),
        PathBuf::from("/Users/demo/projects/shop"),
        Config {
            projects_root: "~/code".to_owned(),
            ..Config::default()
        },
    );
    assert_eq!(
        press(&mut app, 'm'),
        vec![Effect::ScanProjectsRoot(PathBuf::from("/Users/demo/code"))]
    );
    typed(&mut app, "newapp");
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("→ ~/code/newapp"), "{text}");
    // 空着按 ⏎ 不做事、输入框留着。
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert!(matches!(app.prompt, Some(Prompt::NewDir { .. })));
}

// ---- 目录不存在：确认后新建 ----

#[test]
fn missing_dir_asks_then_creates() {
    let root = tmp("create");
    let mut app = App::new(
        PathBuf::from(HOME),
        root.clone(),
        Config {
            projects_root: root.to_string_lossy().into_owned(),
            ..Config::default()
        },
    );
    press(&mut app, 'm');
    typed(&mut app, "fresh");
    assert!(app.key(key(KeyCode::Enter)).is_empty(), "先问，不新建");
    let want = root.join("fresh");
    assert_eq!(
        app.prompt,
        Some(Prompt::ConfirmCreate {
            vendor: Vendor::Claude,
            dir: want.clone()
        })
    );
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("新建目录 /"), "{text}");
    assert!(text.contains("⏎ 确认 · Esc 取消"), "{text}");
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::CreateDir {
            vendor: Vendor::Claude,
            cwd: want.clone()
        }]
    );
    assert!(app.prompt.is_none());
    assert!(!want.exists(), "界面层不建目录，交给运行层");

    // Esc 取消。
    press(&mut app, 'm');
    typed(&mut app, "fresh");
    app.key(key(KeyCode::Enter));
    assert!(app.key(key(KeyCode::Esc)).is_empty());
    assert!(app.prompt.is_none());

    // 已经有的目录直接新对话；是文件的说明一句。
    fs::create_dir_all(&want).unwrap();
    press(&mut app, 'm');
    typed(&mut app, "fresh");
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::New {
            vendor: Vendor::Claude,
            cwd: want
        }]
    );
    fs::write(root.join("file"), "x").unwrap();
    press(&mut app, 'm');
    typed(&mut app, "file");
    assert!(app.key(key(KeyCode::Enter)).is_empty());
    assert!(app.status.as_deref().unwrap().contains("这不是目录"));
    fs::remove_dir_all(&root).unwrap();
}

#[test]
fn candidate_list_is_drawn_above_bottom_bar() {
    let mut app = demo_app(Vendor::Claude);
    press(&mut app, 'm');
    typed(&mut app, "api");
    app.key(key(KeyCode::Down));
    let text = dump(&render(&mut app, 120, 30));
    let lines: Vec<&str> = text.lines().collect();
    // 底栏是最后一行，上面依次是两个候选，再上面是标题行。
    assert!(lines[29].contains("在哪个目录新建 Claude 对话"), "{text}");
    assert!(lines[29].contains("Tab 补全 · ↑↓ 选"), "{text}");
    assert!(lines[28].contains("~/work/api"), "{text}");
    assert!(lines[27].contains("~/projects/api"), "{text}");
    assert!(
        lines[26].contains("目录") && lines[26].contains("2 个"),
        "{text}"
    );
}

// ---- e：项目显示名 ----

fn projects(entries: &[(&str, ProjectNote)]) -> HashMap<PathBuf, ProjectNote> {
    entries
        .iter()
        .map(|(d, n)| (PathBuf::from(d), n.clone()))
        .collect()
}

fn named(name: &str) -> ProjectNote {
    ProjectNote {
        name: Some(name.to_owned()),
        ..ProjectNote::default()
    }
}

#[test]
fn e_on_project_header_renames_project() {
    let mut app = demo_app(Vendor::Claude);
    select_header(&mut app, "/Users/demo/projects/blog");
    press(&mut app, 'e');
    assert_eq!(
        app.prompt,
        Some(Prompt::RenameProject {
            project: PathBuf::from("/Users/demo/projects/blog"),
            text: "blog".to_owned()
        })
    );
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("项目显示名  blog"), "{text}");
    assert!(text.contains("留空恢复目录名"), "{text}");
    for _ in 0..4 {
        app.key(key(KeyCode::Backspace));
    }
    typed(&mut app, "博客");
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::AnnotateProject {
            project: PathBuf::from("/Users/demo/projects/blog"),
            edit: ProjectEdit::Rename(Some("博客".to_owned()))
        }]
    );
    // 清空恢复原名。
    press(&mut app, 'e');
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(
        app.key(key(KeyCode::Enter)),
        vec![Effect::AnnotateProject {
            project: PathBuf::from("/Users/demo/projects/blog"),
            edit: ProjectEdit::Rename(None)
        }]
    );
    // 对话上的 e 还是改对话的名字；非项目组标题上说一句。
    app.key(key(KeyCode::Right));
    app.key(key(KeyCode::Down));
    press(&mut app, 'e');
    assert!(matches!(app.prompt, Some(Prompt::Rename { .. })));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::Home));
    assert!(press(&mut app, 'e').is_empty());
    assert!(app.prompt.is_none());
    assert_eq!(app.status.as_deref(), Some("选中一个项目组再按"));
}

#[test]
fn display_name_shows_in_list_and_preview() {
    let mut app = demo_app(Vendor::Claude);
    app.set_projects(projects(&[
        ("/Users/demo/projects/blog", named("博客")),
        ("/Users/demo/notes", named("笔记本")),
    ]));
    let blog = app
        .list()
        .groups
        .iter()
        .find(|g| g.key == GroupKey::Project("/Users/demo/projects/blog".into()))
        .unwrap();
    assert_eq!(blog.label, "博客");
    select_header(&mut app, "/Users/demo/projects/blog");
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("▸ 博客"), "列表组标题：{text}");
    // 预览：显示名 + 真实路径。
    assert!(text.contains("~/projects/blog"), "{text}");
    // 置顶组里的对话（混着项目）右边写的是显示名。
    assert!(text.contains("笔记本"), "{text}");
    assert_eq!(
        app.project_label(Path::new("/Users/demo/projects/blog")),
        "博客"
    );
    assert_eq!(
        app.project_label(Path::new("/Users/demo/projects/api")),
        "api"
    );
}

#[test]
fn display_name_is_stored_and_used_as_tab_title_prefix() {
    let dir = tmp("label");
    let index = Index::open(&dir, Vec::new()).unwrap();
    let blog = Path::new("/Users/demo/projects/blog");
    let label = |i: &Index| project_label(blog, &i.projects());
    assert_eq!(label(&index), "blog");
    index
        .annotate_project(blog, ProjectEdit::Rename(Some("博客".into())))
        .unwrap();
    // 重开还在；标签页标题和托管标题前缀都拿它拼。
    let index = Index::open(&dir, Vec::new()).unwrap();
    assert_eq!(label(&index), "博客");
    assert_eq!(
        opener::tab_title(Vendor::Claude, &label(&index), Some("RSS 初稿")),
        format!("{} 博客 · RSS 初稿", Vendor::Claude.mark())
    );
    index
        .annotate_project(blog, ProjectEdit::Rename(None))
        .unwrap();
    assert_eq!(
        opener::tab_title(Vendor::Pi, &label(&index), None),
        format!("{} blog", Vendor::Pi.mark())
    );
    fs::remove_dir_all(&dir).unwrap();
}

// ---- a：整组归档 ----

#[test]
fn a_on_project_header_archives_whole_group() {
    let mut app = demo_app(Vendor::Claude);
    select_header(&mut app, "/Users/demo/projects/blog");
    let before: Vec<_> = app.list().entries.iter().map(|e| e.note.clone()).collect();
    assert_eq!(
        press(&mut app, 'a'),
        vec![Effect::AnnotateProject {
            project: PathBuf::from("/Users/demo/projects/blog"),
            edit: ProjectEdit::Archive(true)
        }]
    );
    assert!(app.status.as_deref().unwrap().contains("再按 a 取消"));
    // 运行层存好、换上标注后：blog 排到「归档」后面，不编号，标「已归档」，选中还跟着它。
    app.set_projects(projects(&[(
        "/Users/demo/projects/blog",
        ProjectNote {
            archived: true,
            ..ProjectNote::default()
        },
    )]));
    let groups = &app.list().groups;
    let keys: Vec<&GroupKey> = groups.iter().map(|g| &g.key).collect();
    let at = |k: &GroupKey| keys.iter().position(|x| *x == k).unwrap();
    let blog = GroupKey::Project("/Users/demo/projects/blog".into());
    assert!(at(&blog) > at(&GroupKey::Archived), "{keys:?}");
    let g = &groups[at(&blog)];
    assert_eq!((g.number, g.hint.as_deref()), (None, Some("已归档")));
    assert_eq!(g.items.len(), 2, "组里的对话都跟着");
    // 后面的项目组编号往前挪。
    let api = &groups[at(&GroupKey::Project("/Users/demo/projects/api".into()))];
    assert_eq!(api.number, Some(2));
    assert_eq!(app.selected_group().unwrap().key, blog);
    let notes: Vec<_> = app.list().entries.iter().map(|e| e.note.clone()).collect();
    assert_eq!(before, notes, "对话本身的标注不动");
    let text = dump(&render(&mut app, 120, 30));
    assert!(text.contains("blog  已归档"), "{text}");
    assert!(text.contains("a  移出归档"), "{text}");

    // 再按取消。
    assert_eq!(
        press(&mut app, 'a'),
        vec![Effect::AnnotateProject {
            project: PathBuf::from("/Users/demo/projects/blog"),
            edit: ProjectEdit::Archive(false)
        }]
    );
    // 对话上的 a 还是归档那一条；折叠组标题上说一句。
    app.key(key(KeyCode::Home));
    select_id(&mut app, "c-shop-2");
    assert!(matches!(
        press(&mut app, 'a').as_slice(),
        [Effect::Annotate { .. }]
    ));
    app.key(key(KeyCode::Home));
    assert!(press(&mut app, 'a').is_empty());
    assert_eq!(app.status.as_deref(), Some("选中一个项目组再按"));
}
