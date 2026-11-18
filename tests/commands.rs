#[cfg(unix)]
use std::{fs, path::Path};

mod common;
use common::*;

#[cfg(unix)]
fn canonical(path: &Path) -> std::path::PathBuf {
    dunce::canonicalize(path).unwrap()
}

#[test]
fn run_line_shows_quoted_command_text() {
    let r = Repo::new();
    r.config(r#"commands = ["echo hi"]"#);
    let got = r.run(&[]);
    status(&got, 0);
    assert!(
        out(&got).lines().any(|l| l == "run 1/1 echo hi"),
        "{}",
        out(&got)
    );
}

#[cfg(unix)]
#[test]
fn command_text_newline_is_escaped() {
    let r = Repo::new();
    r.config(r#"commands = ["true\ntrue"]"#);
    let got = r.run(&[]);
    status(&got, 0);
    let stdout = out(&got);
    assert!(stdout.contains("run 1/1 true\\ntrue"), "{stdout}");
    assert!(!stdout.lines().any(|l| l == "true"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn commands_run_in_target_with_plugin_env() {
    let r = Repo::new();
    r.config(r#"commands = ["env > env.txt; pwd > pwd.txt"]"#);
    let got = r
        .exe()
        .arg("run")
        .env("GIT_DIR", "/nonexistent")
        .env("GIT_WORK_TREE", "/x")
        .env("GIT_CONFIG_KEY_0", "a")
        .env("GIT_AUTHOR_NAME", "keep")
        .output()
        .unwrap();
    status(&got, 0);
    let env_txt = fs::read_to_string(r.target.join("env.txt")).unwrap();
    let has = |line: String| env_txt.lines().any(|l| l == line);
    assert!(
        has(format!("HERDR_WT_SOURCE={}", r.source.display())),
        "{env_txt}"
    );
    assert!(
        has(format!("HERDR_WT_TARGET={}", r.target.display())),
        "{env_txt}"
    );
    assert!(has("GIT_AUTHOR_NAME=keep".to_string()), "{env_txt}");
    for removed in ["GIT_DIR=", "GIT_WORK_TREE=", "GIT_CONFIG_KEY_0="] {
        assert!(
            !env_txt.lines().any(|l| l.starts_with(removed)),
            "{removed}: {env_txt}"
        );
    }
    let pwd = fs::read_to_string(r.target.join("pwd.txt")).unwrap();
    assert_eq!(canonical(Path::new(pwd.trim_end())), canonical(&r.target));
}

#[test]
fn commands_run_in_order_and_stop_at_first_failure() {
    let r = Repo::new();
    r.config(r#"commands = ["echo one > one", "exit 3", "echo three > three"]"#);
    let got = r.run(&[]);
    status(&got, 3);
    assert!(r.target.join("one").exists());
    assert!(!r.target.join("three").exists());
    assert!(
        err(&got).contains("command failed 2/3 exit 3"),
        "{}",
        err(&got)
    );
    assert!(out(&got).contains("skipped command 3/3"), "{}", out(&got));
}

#[cfg(unix)]
#[test]
fn signal_termination_is_reported() {
    let r = Repo::new();
    r.config(r#"commands = ["kill -9 $$"]"#);
    let got = r.run(&[]);
    assert!(
        err(&got).contains("command failed 1/1 signal 9"),
        "{}",
        err(&got)
    );
    assert!(!err(&got).contains("signal 0"), "{}", err(&got));
}

#[cfg(unix)]
#[test]
fn unknown_command_exits_127() {
    let r = Repo::new();
    r.config(r#"commands = ["definitely-not-a-command-xyz"]"#);
    let got = r.run(&[]);
    assert!(
        err(&got).contains("command failed 1/1 exit 127"),
        "{}",
        err(&got)
    );
}

#[cfg(unix)]
#[test]
fn command_stdin_is_closed() {
    use std::{
        process::Stdio,
        thread::sleep,
        time::{Duration, Instant},
    };
    let r = Repo::new();
    r.config(r#"commands = ["cat > got"]"#);
    let mut child = r
        .exe()
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _stdin = child.stdin.take();
    let deadline = Instant::now() + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("command blocked on open stdin");
        }
        sleep(Duration::from_millis(20));
    };
    assert!(exit.success(), "{exit:?}");
    assert_eq!(fs::read(r.target.join("got")).unwrap(), b"");
}

#[test]
fn no_commands_skips_every_command() {
    let r = Repo::new();
    r.config(r#"commands = ["echo a > a", "echo b > b"]"#);
    let got = r.run(&["--no-commands"]);
    status(&got, 0);
    assert!(out(&got).contains("skipped command 1/2"), "{}", out(&got));
    assert!(out(&got).contains("skipped command 2/2"), "{}", out(&got));
    assert!(!r.target.join("a").exists());
    assert!(!r.target.join("b").exists());
}

#[cfg(unix)]
#[test]
fn file_failure_still_runs_commands_and_returns_2() {
    use std::os::unix::fs::symlink;
    let r = Repo::new();
    let outer = r.base.path().join("outer");
    fs::create_dir(&outer).unwrap();
    symlink(&outer, r.target.join("linked-parent")).unwrap();
    r.file("linked-parent/a", "contents");
    r.config("files = [\"linked-parent/a\"]\ncommands = [\"echo ok > marker\"]");
    let got = r.run(&[]);
    status(&got, 2);
    assert!(r.target.join("marker").exists());
    assert!(!outer.join("a").exists());
}
