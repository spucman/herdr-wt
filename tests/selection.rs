use std::path::Path;
#[cfg(unix)]
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
};

mod common;
use common::*;

fn files_config(files: &[&str]) -> String {
    format!("files = {files:?}\n")
}

#[test]
fn covered_entries_name_the_covering_directory() {
    let r = Repo::new();
    r.config(&files_config(&["dir", "dir/**", "dir/a"]));
    r.file("dir/a", "a");

    let got = r.run(&[]);

    status(&got, 0);
    let covered = Path::new("dir").join("a");
    let expected = format!("covered {} by dir", covered.display());
    assert!(out(&got).contains(&expected), "{}", out(&got));
}

#[test]
fn invalid_entry_error_names_rule_and_entry() {
    let r = Repo::new();
    r.config(&files_config(&["good", "../x"]));
    r.file("good", "g");

    let got = r.run(&[]);

    status(&got, 1);
    assert!(err(&got).contains("files[2] \"../x\""), "{}", err(&got));
    assert!(err(&got).contains("\"..\" is not allowed"), "{}", err(&got));
    assert!(!r.target.join("good").exists());
}

#[test]
fn glob_without_match_warns() {
    let r = Repo::new();
    r.config(&files_config(&["nothing/*"]));

    let got = r.run(&[]);

    status(&got, 0);
    assert!(err(&got).contains("glob matched no files"), "{}", err(&got));
}

#[test]
fn duplicate_entries_warn() {
    let r = Repo::new();
    r.file("a", "a");
    r.config(&files_config(&["a", "a"]));
    let got = r.run(&[]);
    status(&got, 0);
    assert!(err(&got).contains("duplicate files entry"), "{}", err(&got));

    let r = Repo::new();
    r.file("a", "a");
    r.config(&files_config(&["a", "*"]));
    let got = r.run(&[]);
    status(&got, 0);
    assert!(err(&got).contains("duplicate selection"), "{}", err(&got));
}

#[cfg(unix)]
fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[cfg(unix)]
#[test]
fn globs_skip_symlinks_and_nested_repositories() {
    let r = Repo::new();
    r.config(&files_config(&["src/**"]));
    r.file("src/real.txt", "r");
    r.file("elsewhere/f.txt", "f");
    r.file("src/sub/.git", "gitdir: nowhere");
    r.file("src/sub/x.txt", "x");
    symlink("real.txt", r.source.join("src/link.txt")).unwrap();
    symlink(r.source.join("elsewhere"), r.source.join("src/ldir")).unwrap();

    let got = r.run(&[]);

    status(&got, 0);
    assert!(r.target.join("src/real.txt").is_file());
    assert!(fs::symlink_metadata(r.target.join("src/link.txt")).is_err());
    assert!(fs::symlink_metadata(r.target.join("src/ldir")).is_err());
    assert!(!r.target.join("src/sub").exists());
    for link in ["src/link.txt", "src/ldir"] {
        let line = format!(
            "warning: skipped {} (symlink matched by glob; list it literally)",
            Path::new(link).display()
        );
        assert_eq!(count(&err(&got), &line), 1, "{link}: {}", err(&got));
    }
}

#[cfg(unix)]
#[test]
fn glob_symlink_warning_is_printed_once() {
    let r = Repo::new();
    r.config(&files_config(&["*.txt", "**/*.txt"]));
    r.file("real.txt", "r");
    symlink("real.txt", r.source.join("link.txt")).unwrap();

    let got = r.run(&[]);

    status(&got, 0);
    assert_eq!(
        count(&err(&got), "skipped link.txt (symlink"),
        1,
        "{}",
        err(&got)
    );
}

#[cfg(unix)]
#[test]
fn glob_symlink_warning_is_suppressed_when_listed() {
    let r = Repo::new();
    r.config(&files_config(&["link.txt", "*.txt"]));
    r.file("real.txt", "r");
    symlink("real.txt", r.source.join("link.txt")).unwrap();

    let got = r.run(&[]);

    status(&got, 0);
    assert!(
        !err(&got).contains("symlink matched by glob"),
        "{}",
        err(&got)
    );
    assert!(fs::symlink_metadata(r.target.join("link.txt")).is_ok());

    let r = Repo::new();
    r.config(&files_config(&["d", "d/*"]));
    r.file("d/f", "f");
    symlink("f", r.source.join("d/l")).unwrap();

    let got = r.run(&[]);

    status(&got, 0);
    assert!(
        !err(&got).contains("symlink matched by glob"),
        "{}",
        err(&got)
    );
}

#[cfg(unix)]
#[test]
fn unreadable_glob_directory_is_a_preflight_error() {
    if is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    let r = Repo::new();
    r.config("files = [\"d/**\", \"ok\"]\ncommands = [\"echo x > sentinel\"]\n");
    r.file("ok", "o");
    r.file("d/f", "f");
    let locked = r.source.join("d/locked");
    fs::create_dir(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let got = r.run(&[]);

    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    status(&got, 1);
    assert!(err(&got).contains("locked"), "{}", err(&got));
    assert!(!r.target.join("ok").exists());
    assert!(!r.target.join("sentinel").exists());
}
