use std::fs;

mod common;
use common::*;

#[test]
fn run_in_target_subdirectory_uses_worktree_root() {
    let r = Repo::new();
    r.config("files = [\"item\"]");
    r.file("item", "value");
    let sub = r.target.join("sub");
    fs::create_dir(&sub).unwrap();
    let o = r.exe().arg("run").current_dir(&sub).output().unwrap();
    status(&o, 0);
    assert_eq!(fs::read_to_string(r.target.join("item")).unwrap(), "value");
    assert!(!sub.join("item").exists());
}

#[test]
fn run_in_source_subdirectory_is_refused() {
    let r = Repo::new();
    r.config("files = [\"item\"]");
    r.file("item", "value");
    let sub = r.source.join("sub");
    fs::create_dir(&sub).unwrap();
    let o = r.exe().arg("run").current_dir(&sub).output().unwrap();
    status(&o, 4);
    assert!(!r.target.join("item").exists());
}

#[test]
fn run_on_bare_git_dir_fails() {
    let r = Repo::new();
    let bare = r.base.path().join("bare");
    git(
        r.base.path(),
        &[
            "clone",
            "--bare",
            r.source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    status(&r.exe().arg("run").arg(&bare).output().unwrap(), 1);
}

#[test]
fn run_on_dot_git_dir_fails() {
    let r = Repo::new();
    let dot_git = r.source.join(".git");
    status(&r.exe().arg("run").arg(&dot_git).output().unwrap(), 1);
}

#[test]
fn bare_head_ignores_tag_named_like_branch() {
    let r = Repo::new();
    let root = dunce::canonicalize(r.base.path()).unwrap();
    let bare = root.join("bare");
    let trg = root.join("other");
    git(
        &root,
        &[
            "clone",
            "--bare",
            r.source.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    git(&bare, &["tag", "main"]);
    git(
        &bare,
        &["worktree", "add", "--detach", trg.to_str().unwrap()],
    );
    let o = r.exe().arg("run").arg(&trg).output().unwrap();
    status(&o, 0);
    assert!(
        err(&o).contains("HEAD branch has no checkout"),
        "{}",
        err(&o)
    );
}

#[test]
fn inherited_git_dir_does_not_redirect_discovery() {
    let r = Repo::new();
    r.config("files = [\"item\"]");
    r.file("item", "from source");
    let other = r.base.path().join("unrelated");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "-b", "main"]);
    let o = r
        .exe()
        .arg("run")
        .env("GIT_DIR", other.join(".git"))
        .output()
        .unwrap();
    status(&o, 0);
    assert_eq!(
        fs::read_to_string(r.target.join("item")).unwrap(),
        "from source"
    );
}
