mod common;
use common::*;
use std::fs;

fn run_args(args: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    exe_in(dir.path()).args(args).output().unwrap()
}

fn assert_usage_error(args: &[&str], expected: &str) {
    let o = run_args(args);
    status(&o, 1);
    assert!(err(&o).contains(expected), "{args:?}: {}", err(&o));
    assert!(err(&o).contains("Usage:"), "{args:?}: {}", err(&o));
}

#[test]
fn no_arguments_is_usage_error() {
    let o = run_args(&[]);

    status(&o, 1);
    assert!(err(&o).contains("missing subcommand"), "{}", err(&o));
    assert!(err(&o).contains("Usage: herdr-wt run"), "{}", err(&o));
}

#[test]
fn unknown_command_is_usage_error() {
    assert_usage_error(&["bogus"], "unknown command bogus");
}

#[test]
fn unknown_option_is_usage_error() {
    assert_usage_error(&["--bogus"], "unknown option --bogus");
    assert_usage_error(&["run", "--bogus"], "unknown option --bogus");
}

#[test]
fn version_is_top_level_only() {
    assert_usage_error(&["run", "--version"], "unknown option --version");
    assert_usage_error(&["run", "-V"], "unknown option -V");
}

#[test]
fn second_path_is_usage_error() {
    assert_usage_error(&["run", "a", "b"], "unexpected argument b");
}

#[test]
fn event_and_action_take_no_arguments() {
    for args in [
        &["event", "x"][..],
        &["event", "--help"],
        &["action", "--help"],
    ] {
        assert_usage_error(args, "takes no arguments");
    }
}

#[test]
fn usage_error_escapes_control_characters() {
    let o = run_args(&["--\u{1b}x"]);

    status(&o, 1);
    assert!(err(&o).contains("\\u{1b}"), "{}", err(&o));
    assert!(!o.stderr.contains(&0x1b), "{}", err(&o));
}

#[test]
fn help_prints_usage_on_stdout() {
    for args in [&["--help"][..], &["-h"], &["run", "--help"]] {
        let o = run_args(args);

        status(&o, 0);
        assert!(out(&o).starts_with("Usage:"), "{args:?}: {}", out(&o));
        assert!(o.stderr.is_empty(), "{args:?}: {}", err(&o));
    }
}

#[test]
fn version_prints_name_and_version() {
    let expected = format!("herdr-wt {}", env!("CARGO_PKG_VERSION"));
    for args in [&["--version"][..], &["-V"]] {
        let o = run_args(args);

        status(&o, 0);
        assert_eq!(out(&o).trim_end(), expected, "{args:?}");
        #[cfg(unix)]
        assert_eq!(out(&o), format!("{expected}\n"), "{args:?}");
    }
}

#[test]
fn double_dash_makes_help_a_path() {
    let o = run_args(&["run", "--", "--help"]);

    status(&o, 1);
    assert!(!out(&o).contains("Usage:"), "{}", out(&o));
}

#[test]
fn run_outside_git_fails() {
    let dir = tempfile::tempdir().unwrap();
    let o = exe_in(dir.path())
        .arg("run")
        .arg(dir.path())
        .output()
        .unwrap();

    status(&o, 1);
}

#[test]
fn run_nonexistent_path_fails() {
    let dir = tempfile::tempdir().unwrap();
    let o = exe_in(dir.path())
        .arg("run")
        .arg(dir.path().join("missing"))
        .output()
        .unwrap();

    status(&o, 1);
}

#[cfg(unix)]
#[test]
fn broken_stdout_exits_one() {
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let (r, w) = std::io::pipe().unwrap();
    drop(r);

    let o = exe_in(dir.path())
        .arg("--help")
        .stdout(w)
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    status(&o, 1);
    assert!(err(&o).contains("internal error"), "{}", err(&o));
}

#[test]
fn dry_run_env_requires_exactly_one() {
    let r = Repo::new();
    r.file("a", "new");
    r.config(r#"files = ["a"]"#);

    let dry = r
        .exe()
        .arg("run")
        .env("HERDR_WT_DRY_RUN", "1")
        .output()
        .unwrap();
    status(&dry, 0);
    assert!(out(&dry).contains("would copy"), "{}", out(&dry));
    assert!(!r.target.join("a").exists());

    let real = r
        .exe()
        .arg("run")
        .env("HERDR_WT_DRY_RUN", "true")
        .output()
        .unwrap();
    status(&real, 0);
    assert_eq!(fs::read_to_string(r.target.join("a")).unwrap(), "new");
}
