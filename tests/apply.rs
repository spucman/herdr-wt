mod common;
use common::*;
use std::fs;

#[test]
fn fresh_destination_is_copied() {
    let r = Repo::new();
    r.file("a", "new");
    r.config(r#"files = ["a"]"#);
    let o = r.run(&[]);
    status(&o, 0);
    assert!(out(&o).contains("copied a"), "{}", out(&o));
    assert_eq!(fs::read_to_string(r.target.join("a")).unwrap(), "new");
}

#[test]
fn overwrite_reports_replaced_and_dry_run_would_replace() {
    let r = Repo::new();
    r.file("f", "new");
    r.file("d/x", "new");
    r.config("overwrite = true\nfiles = [\"f\", \"d\"]");
    fs::write(r.target.join("f"), "old").unwrap();
    fs::create_dir(r.target.join("d")).unwrap();
    fs::write(r.target.join("d/old"), "old").unwrap();

    let dry = r.run(&["--dry-run"]);
    status(&dry, 0);
    assert!(out(&dry).contains("would replace f"), "{}", out(&dry));
    assert!(out(&dry).contains("would replace d"), "{}", out(&dry));
    assert_eq!(fs::read_to_string(r.target.join("f")).unwrap(), "old");
    assert!(r.target.join("d/old").exists());
    assert!(!r.target.join("d/x").exists());

    let real = r.run(&[]);
    status(&real, 0);
    assert!(out(&real).contains("replaced f"), "{}", out(&real));
    assert!(out(&real).contains("replaced d"), "{}", out(&real));
    assert_eq!(fs::read_to_string(r.target.join("f")).unwrap(), "new");
    assert_eq!(fs::read_to_string(r.target.join("d/x")).unwrap(), "new");
    assert!(!r.target.join("d/old").exists());
}

#[cfg(unix)]
#[test]
fn symlink_mode_overwrite_reports_replaced() {
    let r = Repo::new();
    r.file("f", "new");
    r.config("mode = \"symlink\"\noverwrite = true\nfiles = [\"f\"]");
    fs::write(r.target.join("f"), "old").unwrap();
    let o = r.run(&[]);
    status(&o, 0);
    assert!(out(&o).contains("replaced f"), "{}", out(&o));
    assert!(r.target.join("f").is_symlink());
}

#[cfg(unix)]
#[test]
fn read_only_existing_dir_is_not_replaced() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    let mode = |p: std::path::PathBuf| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let r = Repo::new();
    r.file("d/f", "new");
    r.file("other", "new");
    r.config("overwrite = true\nfiles = [\"d\", \"other\"]");
    fs::create_dir(r.target.join("d")).unwrap();
    fs::write(r.target.join("d/f"), "old").unwrap();
    fs::set_permissions(r.target.join("d"), fs::Permissions::from_mode(0o500)).unwrap();

    let real = r.run(&[]);
    let dry = r.run(&["--dry-run"]);
    let after_real = (
        fs::read_to_string(r.target.join("d/f")).unwrap(),
        mode(r.target.join("d")),
        fs::read_to_string(r.target.join("other")).ok(),
    );
    let stage_left = fs::read_dir(&r.target).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".herdr-wt-tmp-")
    });
    fs::set_permissions(r.target.join("d"), fs::Permissions::from_mode(0o755)).unwrap();

    status(&real, 2);
    assert!(
        err(&real).contains("cannot replace d (read-only)"),
        "{}",
        err(&real)
    );
    assert_eq!(after_real.0, "old");
    assert_eq!(after_real.1, 0o500);
    assert_eq!(after_real.2.as_deref(), Some("new"));
    assert!(!stage_left);
    status(&dry, 2);
    assert!(
        err(&dry).contains("cannot replace d (read-only)"),
        "{}",
        err(&dry)
    );
}

#[cfg(unix)]
fn run_bounded(r: &Repo, extra_env: &[(&str, &str)]) -> std::process::Output {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = r
        .exe()
        .arg("run")
        .envs(extra_env.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("herdr-wt blocked on a special file");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn mode_of(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
#[test]
fn top_level_fifo_is_skipped_without_blocking() {
    let r = Repo::new();
    mkfifo(&r.source.join("fifo"));
    r.config(r#"files = ["fifo"]"#);
    let o = run_bounded(&r, &[]);
    status(&o, 0);
    assert!(err(&o).contains("skipped special file"), "{}", err(&o));
    assert!(fs::symlink_metadata(r.target.join("fifo")).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_to_fifo_is_skipped() {
    let r = Repo::new();
    mkfifo(&r.source.join("fifo"));
    std::os::unix::fs::symlink("fifo", r.source.join("link")).unwrap();
    r.config(r#"files = ["link"]"#);
    let o = run_bounded(&r, &[]);
    assert!(fs::symlink_metadata(r.target.join("link")).is_err());
    assert!(fs::symlink_metadata(r.target.join("fifo")).is_err());
    status(&o, 0);
}

#[cfg(unix)]
#[test]
fn fifo_inside_copied_dir_is_skipped() {
    let r = Repo::new();
    r.file("dir/f", "data");
    mkfifo(&r.source.join("dir/fifo"));
    r.config(r#"files = ["dir"]"#);
    let o = run_bounded(&r, &[]);
    status(&o, 0);
    assert!(err(&o).contains("skipped special file"), "{}", err(&o));
    assert_eq!(fs::read_to_string(r.target.join("dir/f")).unwrap(), "data");
    assert!(fs::symlink_metadata(r.target.join("dir/fifo")).is_err());
}

#[cfg(unix)]
#[test]
fn fifo_is_never_matched_by_glob() {
    let r = Repo::new();
    r.file("keep", "data");
    mkfifo(&r.source.join("fifo"));
    r.config(r#"files = ["*"]"#);
    let o = run_bounded(&r, &[]);
    status(&o, 0);
    assert!(!err(&o).contains("skipped special file"), "{}", err(&o));
    assert!(r.target.join("keep").exists());
    assert!(fs::symlink_metadata(r.target.join("fifo")).is_err());
}

#[cfg(unix)]
#[test]
fn top_level_file_drops_setuid_setgid() {
    let r = Repo::new();
    r.file("suid", "data");
    set_mode(&r.source.join("suid"), 0o6755);
    r.config(r#"files = ["suid"]"#);
    status(&r.run(&[]), 0);
    assert_eq!(mode_of(&r.target.join("suid")), 0o755);
}

#[cfg(unix)]
#[test]
fn read_only_file_is_copied_with_its_mode() {
    let r = Repo::new();
    r.file("ro", "data");
    set_mode(&r.source.join("ro"), 0o400);
    r.config("overwrite = true\nfiles = [\"ro\"]");
    status(&r.run(&[]), 0);
    assert_eq!(fs::read_to_string(r.target.join("ro")).unwrap(), "data");
    assert_eq!(mode_of(&r.target.join("ro")), 0o400);

    let again = r.run(&[]);
    status(&again, 0);
    assert!(out(&again).contains("replaced ro"), "{}", out(&again));
    assert_eq!(mode_of(&r.target.join("ro")), 0o400);
}

#[cfg(unix)]
#[test]
fn dry_run_creates_no_parent_directories() {
    let r = Repo::new();
    r.file("a/b/c", "data");
    r.config(r#"files = ["a/b/c"]"#);
    let o = run_bounded(&r, &[("HERDR_WT_DRY_RUN", "1")]);
    status(&o, 0);
    assert!(out(&o).contains("would copy a/b/c"), "{}", out(&o));
    assert!(fs::symlink_metadata(r.target.join("a")).is_err());
}
