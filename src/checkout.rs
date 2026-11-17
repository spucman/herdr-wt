use crate::quote;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

pub struct Checkouts {
    pub source: PathBuf,
    pub target: PathBuf,
}

pub fn git_env_is_local(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("GIT_CONFIG_KEY_")
        || key.starts_with("GIT_CONFIG_VALUE_")
        || matches!(
            key.as_str(),
            "GIT_DIR"
                | "GIT_WORK_TREE"
                | "GIT_COMMON_DIR"
                | "GIT_INDEX_FILE"
                | "GIT_PREFIX"
                | "GIT_OBJECT_DIRECTORY"
                | "GIT_ALTERNATE_OBJECT_DIRECTORIES"
                | "GIT_GRAFT_FILE"
                | "GIT_SHALLOW_FILE"
                | "GIT_REPLACE_REF_BASE"
                | "GIT_NO_REPLACE_OBJECTS"
                | "GIT_IMPLICIT_WORK_TREE"
                | "GIT_CONFIG"
                | "GIT_CONFIG_PARAMETERS"
                | "GIT_CONFIG_COUNT"
                | "GIT_INTERNAL_SUPER_PREFIX"
                | "GIT_QUARANTINE_PATH"
        )
}

pub fn clean_git_env(command: &mut Command) {
    for (key, _) in env::vars_os() {
        if key.to_str().is_some_and(git_env_is_local) {
            command.env_remove(key);
        }
    }
}

pub fn git(path: &Path, args: &[&str]) -> Result<Output> {
    let mut command = Command::new("git");
    clean_git_env(&mut command);
    command
        .args(args)
        .current_dir(path)
        .stdin(Stdio::null())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .context("run Git (is git on PATH?)")
}

fn git_stdout(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = git(path, args)?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn git_path(bytes: &[u8]) -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::str::from_utf8(bytes).context("checkout paths must be Unicode")?,
    ))
}

fn root(path: &Path) -> Result<PathBuf> {
    let canon = dunce::canonicalize(path).with_context(|| format!("resolve {}", quote(path)))?;
    if !canon.is_dir() || canon.to_str().is_none() {
        bail!("checkout must be a directory with a Unicode path");
    }
    Ok(canon)
}

pub fn same(a: &Path, b: &Path) -> io::Result<bool> {
    if a == b {
        return Ok(true);
    }
    same_file::is_same_file(a, b)
}

pub fn standalone(path: &Path) -> Result<Option<Checkouts>> {
    let path = root(path)?;
    let mut top = git_stdout(&path, &["rev-parse", "--show-toplevel"])?;
    if top.last() == Some(&b'\n') {
        top.pop();
    }
    let target = root(&git_path(&top)?)?;
    discover(target.clone(), &target)
}

#[derive(Default, Deserialize)]
struct Provenance {
    checkout_path: Option<String>,
    repo_root: Option<String>,
}

#[derive(Deserialize)]
struct Workspace {
    worktree: Option<Provenance>,
}

#[derive(Deserialize)]
struct Event {
    event: String,
    data: EventData,
}

#[derive(Deserialize)]
struct EventData {
    worktree: Option<EventWorktree>,
    workspace: Option<Workspace>,
}

#[derive(Deserialize)]
struct EventWorktree {
    path: Option<String>,
}

fn host_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() || path.contains('\u{fffd}') {
        bail!("invalid or lossy Herdr checkout path; use herdr-wt run [PATH]");
    }
    root(p)
}

pub fn herdr(event: bool) -> Result<Option<Checkouts>> {
    let (target, source) = if event {
        let raw = env::var("HERDR_PLUGIN_EVENT_JSON").context("missing Herdr event payload")?;
        let event: Event = serde_json::from_str(&raw).context("invalid Herdr event payload")?;
        if !matches!(
            event.event.as_str(),
            "worktree_created" | "worktree.created"
        ) {
            bail!("expected worktree.created event");
        }
        let ws = event
            .data
            .workspace
            .and_then(|w| w.worktree)
            .unwrap_or_default();
        (
            event
                .data
                .worktree
                .and_then(|t| t.path)
                .or(ws.checkout_path),
            ws.repo_root,
        )
    } else {
        let raw = env::var("HERDR_PLUGIN_CONTEXT_JSON").context("missing Herdr action context")?;
        let context: Workspace =
            serde_json::from_str(&raw).context("invalid Herdr action context")?;
        let ws = context.worktree.unwrap_or_default();
        (ws.checkout_path, ws.repo_root)
    };
    let target =
        host_path(&target.ok_or_else(|| {
            anyhow!("workspace has no recorded worktree; use herdr-wt run [PATH]")
        })?)?;
    let Some(source) = source else {
        return discover(target.clone(), &target);
    };
    let source = host_path(&source)?;
    if fs::symlink_metadata(source.join(".git")).is_ok() {
        return Ok(Some(Checkouts { source, target }));
    }
    // A bare repository is not itself a checkout.
    discover(target, &source)
}

#[derive(Default)]
struct Record<'a> {
    path: Option<&'a [u8]>,
    branch: Option<&'a [u8]>,
    bare: bool,
    prunable: bool,
}

fn records(bytes: &[u8]) -> Vec<Record<'_>> {
    let mut records = Vec::new();
    let mut r = Record::default();
    for field in bytes.split(|b| *b == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            r.path = Some(path);
        } else if let Some(branch) = field.strip_prefix(b"branch ") {
            r.branch = Some(branch);
        } else if field == b"bare" {
            r.bare = true;
        } else if field == b"prunable" || field.starts_with(b"prunable ") {
            r.prunable = true;
        } else if field.is_empty() && r.path.is_some() {
            records.push(std::mem::take(&mut r));
        }
    }
    records
}

fn discover(target: PathBuf, query: &Path) -> Result<Option<Checkouts>> {
    let list = git_stdout(query, &["worktree", "list", "--porcelain", "-z"])
        .context("worktree discovery requires Git 2.36+")?;
    let entries = records(&list);
    let first = entries
        .first()
        .ok_or_else(|| anyhow!("Git returned no worktrees"))?;
    let main = root(&git_path(first.path.expect("record has path"))?)?;
    let mut registered = false;
    for entry in entries
        .iter()
        .filter(|entry| !entry.bare && !entry.prunable)
    {
        let Ok(path) = git_path(entry.path.expect("record has path")) else {
            continue;
        };
        if path.is_dir() && same(&target, &path).unwrap_or(false) {
            registered = true;
            break;
        }
    }
    if !registered {
        bail!(
            "target is not a registered Git worktree: {}",
            quote(&target)
        );
    }
    if !first.bare {
        if fs::symlink_metadata(main.join(".git")).is_err() {
            bail!("Git did not identify a main checkout");
        }
        return Ok(Some(Checkouts {
            source: main,
            target,
        }));
    }
    let head = git(&main, &["symbolic-ref", "--quiet", "HEAD"])?;
    let mut candidates = Vec::new();
    if head.status.success() {
        let branch = head.stdout.strip_suffix(b"\n").unwrap_or(&head.stdout);
        for rec in entries
            .iter()
            .skip(1)
            .filter(|r| !r.bare && !r.prunable && r.branch == Some(branch))
        {
            let path = git_path(rec.path.expect("record has path"))?;
            if path.is_dir() {
                candidates.push(root(&path)?);
            }
        }
    } else if head.status.code() != Some(1) {
        bail!(
            "cannot resolve bare HEAD: {}",
            String::from_utf8_lossy(&head.stderr).trim()
        );
    }
    match candidates.len() {
        0 => {
            eprintln!("warning: bare repository HEAD branch has no checkout");
            Ok(None)
        }
        1 => Ok(Some(Checkouts {
            source: candidates.remove(0),
            target,
        })),
        _ => bail!(
            "bare repository HEAD has multiple checkouts: {}",
            candidates
                .iter()
                .map(|p| quote(p))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}
