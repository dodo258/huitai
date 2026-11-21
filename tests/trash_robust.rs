//! 回收站不丢数据：同名冲突各占一条、恢复绝不覆盖原处已有的东西、权限不足原文件还在。
//! 中途失败（磁盘满、跨卷、移回也失败）用假的改名注入，在 src/sources/trash.rs 的单元测试里。

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use huitai::core::Vendor;
use huitai::sources::Trash;

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("huitai-trash-it-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// 同一条对话连删很多次（同一毫秒里也有）：每次一条，清单不互相覆盖，全都能恢复。
#[test]
fn rapid_trashes_of_same_id_all_survive() {
    let d = temp_dir("rapid");
    let trash = Trash::new(d.join("trash"));
    let files: Vec<PathBuf> = (0..30)
        .map(|i| {
            let p = d.join(format!("p{i}/same-id.jsonl"));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("第 {i} 份")).unwrap();
            p
        })
        .collect();
    for f in &files {
        trash
            .move_in(Vendor::Claude, "same-id", std::slice::from_ref(f))
            .unwrap();
    }
    let list = trash.list().unwrap();
    assert_eq!(list.len(), files.len(), "同名条目被覆盖了");
    for e in &list {
        trash.restore(e).unwrap();
    }
    for (i, f) in files.iter().enumerate() {
        assert_eq!(fs::read_to_string(f).unwrap(), format!("第 {i} 份"));
    }
}

/// 原处现在是一个断掉的符号链接（或空目录）：恢复报「已存在」，不把它替换掉。
#[test]
fn restore_does_not_replace_dangling_link_or_empty_dir() {
    let d = temp_dir("noclobber");
    let trash = Trash::new(d.join("trash"));
    let file = d.join("s.jsonl");
    fs::write(&file, "原文").unwrap();
    let entry = trash
        .move_in(Vendor::Pi, "s", std::slice::from_ref(&file))
        .unwrap();

    std::os::unix::fs::symlink(d.join("nowhere"), &file).unwrap();
    let err = trash.restore(&entry).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists);
    assert!(
        fs::symlink_metadata(&file)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(&file).unwrap();

    fs::create_dir(&file).unwrap();
    assert_eq!(
        trash.restore(&entry).unwrap_err().kind(),
        ErrorKind::AlreadyExists
    );
    fs::remove_dir(&file).unwrap();

    assert_eq!(trash.list().unwrap().len(), 1, "失败的恢复不能丢条目");
    trash.restore(&entry).unwrap();
    assert_eq!(fs::read_to_string(&file).unwrap(), "原文");
}

/// 对话所在目录只读：删不了，原文件还在，回收站里不留半条。
#[test]
fn permission_denied_keeps_original() {
    // SAFETY: geteuid 没有前置条件。
    if unsafe { libc::geteuid() } == 0 {
        return; // root 不受目录权限限制。
    }
    let d = temp_dir("perm");
    let dir = d.join("proj");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.jsonl");
    let side = dir.join("s");
    fs::write(&file, "keep").unwrap();
    fs::create_dir(&side).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
    let trash = Trash::new(d.join("trash"));
    let res = trash.move_in(Vendor::Claude, "s", &[file.clone(), side.clone()]);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(res.unwrap_err().kind(), ErrorKind::PermissionDenied);
    assert_eq!(fs::read_to_string(&file).unwrap(), "keep");
    assert!(side.is_dir());
    assert!(trash.list().unwrap().is_empty());
    assert_eq!(fs::read_dir(trash.root()).unwrap().count(), 0);
}
