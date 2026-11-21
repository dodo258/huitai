//! 改过数据目录（CLAUDE_CONFIG_DIR / CODEX_HOME / PI_CODING_AGENT_DIR）时，交给守护进程和复制出去的命令带上它。

use std::path::Path;

use huitai::core::{Vendor, VendorDirs};
use huitai::sources::{Command, env_prefix};
use huitai::ui::text::resume_line;

fn dirs(pairs: &'static [(&'static str, &'static str)]) -> VendorDirs {
    VendorDirs::new(Path::new("/h"), Path::new("/h"), move |k| {
        pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.into())
    })
}

#[test]
fn prefix_only_for_the_moved_vendor_and_quoted() {
    let h = Path::new("/h");
    let d = dirs(&[("CODEX_HOME", "/data/my codex")]);
    assert_eq!(
        env_prefix(&d, Vendor::Codex, h),
        "CODEX_HOME='/data/my codex' "
    );
    assert_eq!(env_prefix(&d, Vendor::Claude, h), "");
    assert_eq!(env_prefix(&dirs(&[]), Vendor::Codex, h), "");
}

#[test]
fn copied_resume_line_carries_the_dir() {
    let d = dirs(&[("CLAUDE_CONFIG_DIR", "/h/.claude-work")]);
    let cmd = Command {
        program: "claude".to_owned(),
        args: vec!["--resume".to_owned(), "abc".to_owned()],
        cwd: "/h/projects/shop".into(),
    };
    assert_eq!(
        resume_line(&cmd, &env_prefix(&d, Vendor::Claude, Path::new("/h"))),
        "cd '/h/projects/shop' && CLAUDE_CONFIG_DIR=/h/.claude-work claude --resume abc"
    );
}
