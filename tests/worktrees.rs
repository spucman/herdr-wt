use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn git(cwd: &Path, args: &[&str]) {
    let o = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {}", err(&o));
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn status(o: &Output, expected: i32) {
    assert_eq!(o.status.code(), Some(expected), "{}\n{}", out(o), err(o));
}

struct Repo {
    base: TempDir,
    source: PathBuf,
    target: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(base.path()).unwrap();
        let source = root.join("source checkout");
        let target = root.join("target checkout");
        fs::create_dir(&source).unwrap();
        git(&source, &["init", "-b", "main"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=test@example.org",
                "-c",
                "commit.gpgSign=false",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        git(
            &source,
            &["worktree", "add", "-b", "linked", target.to_str().unwrap()],
        );
        Self {
            base,
            source,
            target,
        }
    }

    fn config(&self, config: &str) {
        self.file(".herdr-wt.toml", config);
    }

    fn file(&self, path: &str, contents: &str) {
        let path = self.source.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn exe(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_herdr-wt"));
        let home = self.base.path().join("home");
        fs::create_dir_all(&home).unwrap();
        c.current_dir(&self.target)
            .env_remove("HERDR_WT_DRY_RUN")
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("LOCALAPPDATA", &home)
            .env("XDG_DATA_HOME", &home);
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.exe().arg("run").args(args).output().unwrap()
    }
}

#[test]
fn reruns_overwrite_and_globs() {
    let r = Repo::new();
    let command = if cfg!(windows) {
        "echo ok>>commands.log"
    } else {
        "echo ok >>commands.log"
    };
    r.config(&format!(
        r#"files = ["dir/**", "dir/nested/a.txt", ".env", "missing"]
commands = ["{command}"]"#
    ));
    r.file("dir/visible.txt", "visible");
    r.file("dir/.hidden", "hidden");
    r.file("dir/nested/a.txt", "nest");
    r.file("dir/other/.git", "nested git pointer");
    r.file("dir/other/unwanted", "not traversed");
    r.file(".env", "first");
    let got = r.run(&["--dry-run"]);
    status(&got, 0);
    assert!(out(&got).contains("would run 1/1"));
    assert!(!r.target.join("dir").exists());
    status(&r.run(&[]), 0);
    assert!(r.target.join("dir/.hidden").is_file());
    assert!(r.target.join("dir/nested/a.txt").is_file());
    assert!(!r.target.join("dir/other").exists());
    r.file(".env", "second");
    status(&r.run(&[]), 0);
    assert_eq!(fs::read_to_string(r.target.join(".env")).unwrap(), "first");
    assert_eq!(
        fs::read_to_string(r.target.join("commands.log"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    r.config(&format!(
        "overwrite = true\n{}",
        fs::read_to_string(r.source.join(".herdr-wt.toml")).unwrap()
    ));
    status(&r.run(&["--no-commands"]), 0);
    assert_eq!(fs::read_to_string(r.target.join(".env")).unwrap(), "second");
    assert_eq!(
        fs::read_to_string(r.target.join("commands.log"))
            .unwrap()
            .lines()
            .count(),
        2
    );
}

#[test]
fn validation_precedes_files_and_commands() {
    let r = Repo::new();
    r.file("good", "content");
    for bad in [
        "../x",
        ".GiT ",
        "GIT~1/a",
        "a//",
        "/root",
        "some**name",
        "bad[",
        "bad]",
        "a:b",
    ] {
        r.config(&format!(
            r#"files = ["good", "{bad}"]
commands = ["echo bad >sentinel"]"#
        ));
        status(&r.run(&[]), 1);
        assert!(!r.target.join("good").exists());
        assert!(!r.target.join("sentinel").exists());
    }
    r.config("unknown = true");
    status(&r.run(&[]), 1);
    r.config("files = [\"good\"]");
    status(&r.exe().arg("run").arg(&r.source).output().unwrap(), 4);
}

#[test]
fn scopes_herdr_invocations_to_provenance() {
    let r = Repo::new();
    r.config("files = [\"sample\"]");
    r.file("sample", "value");
    let payload = serde_json::json!({
        "event": "worktree_created",
        "data": {"workspace": {"worktree": {
            "checkout_path": r.target, "repo_root": r.source
        }}}
    });
    status(
        &r.exe()
            .arg("event")
            .env("HERDR_PLUGIN_EVENT_JSON", payload.to_string())
            .output()
            .unwrap(),
        0,
    );
    assert!(r.target.join("sample").exists());
    fs::remove_file(r.target.join("sample")).unwrap();
    let context = serde_json::json!({"workspace_cwd": r.target, "focused_pane_cwd": r.target});
    let o = r
        .exe()
        .arg("action")
        .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
        .output()
        .unwrap();
    status(&o, 1);
    assert!(err(&o).contains("no recorded worktree"));
    assert!(!r.target.join("sample").exists());
    let context = serde_json::json!({
        "workspace_cwd": r.source,
        "worktree": { "repo_root": r.source, "checkout_path": r.target }
    });
    status(
        &r.exe()
            .arg("action")
            .env("HERDR_PLUGIN_CONTEXT_JSON", context.to_string())
            .output()
            .unwrap(),
        0,
    );
    assert!(r.target.join("sample").exists());
}

#[test]
fn overwrite_and_displaced_original() {
    let r = Repo::new();
    r.config("overwrite = true\nfiles = [\"dir/\", \"leaf\"]");
    r.file("dir/a", "new a");
    r.file("dir/a2", "new a2");
    r.file("dir/nested/.git", "must not be copied");
    r.file("dir/nested/b", "working tree content");
    r.file("leaf", "new leaf");
    fs::write(r.target.join("dir"), "old file").unwrap();
    fs::create_dir(r.target.join("leaf")).unwrap();
    fs::write(r.target.join("leaf/old"), "old dir").unwrap();
    status(&r.run(&[]), 0);
    assert!(!r.target.join("dir/nested/.git").exists());
    assert!(r.target.join("dir/nested/b").is_file());
    assert!(r.target.join("leaf").is_file());

    let spelling = if cfg!(windows) || r.target.join(".GIT").exists() {
        "DIR"
    } else {
        "dir"
    };
    let digest = format!("{:x}", Sha256::digest(spelling.as_bytes()));
    let backup = r
        .target
        .join(format!(".herdr-wt-tmp-original-{digest}"))
        .join("original");
    fs::create_dir_all(&backup).unwrap();
    fs::write(backup.join("previous"), "preserve").unwrap();
    r.file("leaf", "later");
    for args in [&["--dry-run"][..], &[][..]] {
        let result = r.run(args);
        status(&result, 2);
        assert!(err(&result).contains("displaced original"));
        assert!(backup.join("previous").is_file());
    }
    assert_eq!(fs::read_to_string(r.target.join("leaf")).unwrap(), "later");
}

#[test]
fn bare_head_resolves_without_mutating_git() {
    let r = Repo::new();
    let root = dunce::canonicalize(r.base.path()).unwrap();
    let bare = root.join("bare");
    let src = root.join("head");
    let trg = root.join("other");
    let duplicate = root.join("duplicate");
    git(
        &root,
        &[
            "clone",
            "--bare",
            r.source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(
        &bare,
        &["worktree", "add", "--detach", trg.to_str().unwrap()],
    );
    let run = || r.exe().arg("run").arg(&trg).output().unwrap();
    let skipped = run();
    status(&skipped, 0);
    assert!(err(&skipped).contains("HEAD branch has no checkout"));
    git(&bare, &["worktree", "add", src.to_str().unwrap(), "main"]);
    fs::write(src.join(".herdr-wt.toml"), "files = [\"item\"]").unwrap();
    fs::write(src.join("item"), "from HEAD").unwrap();
    status(&run(), 0);
    assert_eq!(fs::read_to_string(trg.join("item")).unwrap(), "from HEAD");
    git(
        &bare,
        &[
            "worktree",
            "add",
            "--force",
            duplicate.to_str().unwrap(),
            "main",
        ],
    );
    let failed = run();
    status(&failed, 1);
    assert!(err(&failed).contains("multiple checkouts"));
}

#[cfg(unix)]
#[test]
fn symlink_boundaries_and_file_failure_precedence() {
    use std::os::unix::fs::symlink;
    let r = Repo::new();
    let outer = r.base.path().join("outer");
    fs::create_dir(&outer).unwrap();
    fs::write(outer.join("secret"), "outside").unwrap();
    r.file("dir/a", "internal");
    symlink("dir/a", r.source.join("relative")).unwrap();
    symlink(r.source.join("dir/a"), r.source.join("absolute")).unwrap();
    symlink(&outer, r.source.join("out")).unwrap();
    r.config("files = [\"absolute\", \"out/secret\"]");
    status(&r.run(&[]), 1);
    assert!(!r.target.join("absolute").exists());
    r.config("files = [\"absolute\", \"relative\", \"dir\", \"*lative\"]");
    symlink(outer.join("secret"), r.source.join("dir/verbatim")).unwrap();
    status(&r.run(&[]), 0);
    assert_eq!(
        fs::read_to_string(r.target.join("absolute")).unwrap(),
        "internal"
    );
    assert_eq!(
        fs::read_link(r.target.join("dir/verbatim")).unwrap(),
        outer.join("secret")
    );
    symlink(&outer, r.target.join("linked-parent")).unwrap();
    r.file("linked-parent/a", "contents");
    r.config("files = [\"linked-parent/a\"]\ncommands = [\"echo handled > marker\", \"exit 7\", \"echo never > later\"]");
    status(&r.run(&["--dry-run"]), 2);
    let result = r.run(&[]);
    status(&result, 3);
    assert!(r.target.join("marker").exists());
    assert!(!outer.join("a").exists());
    assert!(!r.target.join("later").exists());
    assert!(err(&result).contains("command failed 2/3 exit 7"));
}

#[cfg(unix)]
#[test]
fn config_symlink_and_symlink_mode() {
    use std::os::unix::fs::symlink;
    let r = Repo::new();
    r.file("boot.toml", "mode = \"symlink\"\nfiles = [\"sample\"]");
    r.file("sample", "value");
    symlink(r.source.join("boot.toml"), r.source.join(".herdr-wt.toml")).unwrap();
    status(&r.run(&[]), 0);
    assert!(r.target.join("sample").is_symlink());
    assert!(out(&r.run(&[])).contains("already linked"));
    fs::remove_file(r.source.join(".herdr-wt.toml")).unwrap();
    symlink("does-not-exist", r.source.join(".herdr-wt.toml")).unwrap();
    status(&r.run(&[]), 1);
}

#[cfg(unix)]
#[test]
fn lock_spans_commands_but_not_preview() {
    use std::{
        process::Stdio,
        thread::sleep,
        time::{Duration, Instant},
    };
    let r = Repo::new();
    r.config("commands = ['touch started; while [ ! -f finish ]; do sleep 0.05; done']");
    let mut first = r.exe().arg("run").stdout(Stdio::null()).spawn().unwrap();
    let start = Instant::now();
    while !r.target.join("started").exists() && start.elapsed() < Duration::from_secs(5) {
        sleep(Duration::from_millis(25));
    }
    let saw_started = r.target.join("started").exists();
    let busy = r.run(&[]);
    let dry = r.run(&["--dry-run"]);
    fs::write(r.target.join("finish"), "").unwrap();
    assert!(first.wait().unwrap().success());
    assert!(saw_started);
    status(&busy, 1);
    assert!(err(&busy).contains("target busy"));
    status(&dry, 0);
}

#[cfg(unix)]
#[test]
fn copied_tree_preserves_file_mtime_and_strips_special_mode_bits() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};
    let r = Repo::new();
    r.config("files = ['tools']");
    r.file("tools/tool", "test");
    let src = r.source.join("tools/tool");
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    fs::File::open(&src).unwrap().set_modified(mtime).unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o4755)).unwrap();
    fs::set_permissions(r.source.join("tools"), fs::Permissions::from_mode(0o1750)).unwrap();
    status(&r.run(&[]), 0);
    let copied = fs::metadata(r.target.join("tools/tool")).unwrap();
    assert_eq!(copied.modified().unwrap(), mtime);
    assert_eq!(copied.permissions().mode() & 0o7777, 0o755);
    let copied_dir = fs::metadata(r.target.join("tools")).unwrap();
    assert_eq!(copied_dir.permissions().mode() & 0o7777, 0o750);
}

#[cfg(windows)]
#[test]
fn target_lock_uses_local_app_data() {
    let r = Repo::new();
    r.config("files = ['sample']");
    r.file("sample", "value");

    let locks = r.base.path().join("home/herdr-wt/locks");
    status(&r.run(&["--dry-run"]), 0);
    assert!(!locks.exists());
    status(&r.run(&[]), 0);
    assert_eq!(fs::read_dir(locks).unwrap().count(), 1);
    assert_eq!(
        fs::read_to_string(r.target.join("sample")).unwrap(),
        "value"
    );
}
