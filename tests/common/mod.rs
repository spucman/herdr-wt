#![allow(dead_code)]

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

fn remove_git_env(command: &mut Command) {
    let names: Vec<OsString> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_string_lossy().to_uppercase().starts_with("GIT_"))
        .collect();
    for name in names {
        command.env_remove(name);
    }
}

/// `ceiling` stops git's upward repository discovery, so a TMPDIR that lives
/// inside a repository is never mistaken for the test's checkout.
fn isolate(command: &mut Command, dir: &Path, home: &Path, ceiling: &Path) {
    remove_git_env(command);
    command
        .current_dir(dir)
        .env("GIT_CEILING_DIRECTORIES", ceiling)
        .env_remove("HERDR_WT_DRY_RUN")
        .env_remove("HERDR_PLUGIN_EVENT_JSON")
        .env_remove("HERDR_PLUGIN_CONTEXT_JSON")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("LOCALAPPDATA", home)
        .env("XDG_DATA_HOME", home);
}

pub fn git(cwd: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    remove_git_env(&mut command);
    let o = command.output().unwrap();
    assert!(o.status.success(), "{args:?}: {}", err(&o));
}

pub fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn status(o: &Output, expected: i32) {
    assert_eq!(o.status.code(), Some(expected), "{}\n{}", out(o), err(o));
}

pub struct Repo {
    pub base: TempDir,
    pub source: PathBuf,
    pub target: PathBuf,
}

impl Repo {
    pub fn new() -> Self {
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

    pub fn config(&self, config: &str) {
        self.file(".herdr-wt.toml", config);
    }

    pub fn file(&self, path: &str, contents: &str) {
        let path = self.source.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    pub fn exe(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_herdr-wt"));
        let home = self.base.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let ceiling = self.base.path().parent().unwrap_or(self.base.path());
        isolate(&mut c, &self.target, &home, ceiling);
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.exe().arg("run").args(args).output().unwrap()
    }
}

pub fn exe_in(dir: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_herdr-wt"));
    let home = dir.join("home");
    fs::create_dir_all(&home).unwrap();
    isolate(&mut c, dir, &home, dir.parent().unwrap_or(dir));
    c
}

/// True when permission bits are not enforced for this process (root, or a
/// user holding CAP_DAC_OVERRIDE). Probes behavior instead of checking the uid.
#[cfg(unix)]
pub fn is_root() -> bool {
    use std::os::unix::fs::PermissionsExt;
    let set_mode = |path: &Path, mode: u32| {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    };
    let dir = tempfile::tempdir().unwrap();
    let locked = dir.path().join("locked");
    fs::write(&locked, "x").unwrap();
    set_mode(&locked, 0o000);
    set_mode(dir.path(), 0o500);
    let bypasses = fs::read(&locked).is_ok() || fs::write(dir.path().join("new"), "x").is_ok();
    set_mode(dir.path(), 0o700);
    set_mode(&locked, 0o600);
    bypasses
}

#[cfg(unix)]
pub fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo {path:?}");
}
