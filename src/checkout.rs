use crate::quote;
use anyhow::{anyhow, bail, Context, Result};
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
    use crate::fs::{identity, open_root};
    Ok(identity(&open_root(a)?)? == identity(&open_root(b)?)?)
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

fn host_path(path: &str) -> Result<PathBuf> {
    let p = Path::new(path);
    if !p.is_absolute() || path.contains('\u{fffd}') {
        bail!("invalid or lossy Herdr checkout path; use herdr-wt run [PATH]");
    }
    root(p)
}

fn text(v: &serde_json::Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn payload(name: &str, what: &str) -> Result<String> {
    match env::var(name) {
        Ok(raw) => Ok(raw),
        Err(env::VarError::NotPresent) => bail!("missing {what} ({name} is not set)"),
        Err(env::VarError::NotUnicode(_)) => bail!("invalid {what}: {name} is not Unicode"),
    }
}

fn event_paths(raw: &str) -> Result<(Option<String>, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(raw).context("invalid Herdr event payload")?;
    match v.get("event") {
        None => bail!("invalid Herdr event payload: missing event name"),
        Some(serde_json::Value::String(s))
            if matches!(s.as_str(), "worktree.created" | "worktree_created") => {}
        Some(serde_json::Value::String(s)) => bail!("expected worktree.created event, got {s:?}"),
        Some(other) => bail!("expected worktree.created event, got {other}"),
    }
    let target = text(&v, "/data/worktree/path")
        .or_else(|| text(&v, "/data/workspace/worktree/checkout_path"));
    Ok((target, text(&v, "/data/workspace/worktree/repo_root")))
}

fn context_paths(raw: &str) -> Result<(Option<String>, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(raw).context("invalid Herdr action context")?;
    Ok((
        text(&v, "/worktree/checkout_path"),
        text(&v, "/worktree/repo_root"),
    ))
}

pub fn herdr(event: bool) -> Result<Option<Checkouts>> {
    let (target, source) = if event {
        event_paths(&payload("HERDR_PLUGIN_EVENT_JSON", "Herdr event payload")?)?
    } else {
        context_paths(&payload(
            "HERDR_PLUGIN_CONTEXT_JSON",
            "Herdr action context",
        )?)?
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EVENT_FIXTURE: &str = include_str!("../tests/fixtures/worktree_created.json");
    const CONTEXT_FIXTURE: &str = include_str!("../tests/fixtures/action_context.json");

    fn event(value: serde_json::Value) -> Result<(Option<String>, Option<String>)> {
        event_paths(&value.to_string())
    }

    fn event_error(value: serde_json::Value) -> String {
        event(value)
            .expect_err("event must be rejected")
            .to_string()
    }

    fn some(s: &str) -> Option<String> {
        Some(s.to_owned())
    }

    #[test]
    fn event_reads_real_fixture() {
        assert_eq!(
            event_paths(EVENT_FIXTURE).unwrap(),
            (some("/REPLACE/worktree"), some("/REPLACE/main"))
        );
    }

    #[test]
    fn context_reads_real_fixture() {
        assert_eq!(
            context_paths(CONTEXT_FIXTURE).unwrap(),
            (some("/REPLACE/worktree"), some("/REPLACE/main"))
        );
    }

    #[test]
    fn event_prefers_worktree_path() {
        let (target, _) = event(json!({
            "event": "worktree.created",
            "data": {
                "worktree": {"path": "/direct"},
                "workspace": {"worktree": {"checkout_path": "/fallback"}}
            }
        }))
        .unwrap();
        assert_eq!(target, some("/direct"));
    }

    #[test]
    fn event_falls_back_when_path_is_null_empty_missing_or_not_a_string() {
        let worktrees = [
            json!({"path": null}),
            json!({"path": ""}),
            json!({}),
            json!({"path": 42}),
        ];
        for worktree in worktrees {
            let (target, _) = event(json!({
                "event": "worktree.created",
                "data": {
                    "worktree": worktree,
                    "workspace": {"worktree": {"checkout_path": "/fallback"}}
                }
            }))
            .unwrap();
            assert_eq!(target, some("/fallback"), "worktree: {worktree}");
        }
    }

    #[test]
    fn event_without_any_path_is_none() {
        let (target, _) = event(json!({"event": "worktree_created", "data": {}})).unwrap();
        assert_eq!(target, None);
    }

    #[test]
    fn event_requires_known_name() {
        assert!(event_error(json!({"data": {}})).contains("missing event name"));
        assert!(
            event_error(json!({"event": "worktree.removed"})).contains("got \"worktree.removed\"")
        );
        assert!(event_error(json!({"event": 42})).contains("got 42"));
        assert!(event_error(json!({"event": null})).contains("got null"));
        let escaped = event_error(json!({"event": "a\nb"}));
        assert!(escaped.contains("got \"a\\nb\""));
        assert!(!escaped.contains('\n'));
        assert!(event(json!({"event": "worktree.created"})).is_ok());
        assert!(event(json!({"event": "worktree_created"})).is_ok());
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(event_paths("not json").is_err());
    }

    #[test]
    fn context_with_null_worktree_is_none() {
        assert_eq!(
            context_paths(r#"{"worktree": null}"#).unwrap(),
            (None, None)
        );
    }

    #[test]
    fn non_string_repo_root_is_ignored() {
        let (_, source) = event(json!({
            "event": "worktree.created",
            "data": {"workspace": {"worktree": {"repo_root": 5}}}
        }))
        .unwrap();
        assert_eq!(source, None);
    }

    #[test]
    fn records_parse_bare_branch_detached_prunable_and_spaces() {
        let input = b"worktree /a b\0HEAD 1\0branch refs/heads/main\0\0worktree /bare\0bare\0\0worktree /p\0detached\0prunable gitdir file points to non-existent location\0\0";
        let parsed = records(input);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].path, Some(&b"/a b"[..]));
        assert_eq!(parsed[0].branch, Some(&b"refs/heads/main"[..]));
        assert!(!parsed[0].bare);
        assert!(!parsed[0].prunable);
        assert_eq!(parsed[1].path, Some(&b"/bare"[..]));
        assert!(parsed[1].bare);
        assert_eq!(parsed[2].path, Some(&b"/p"[..]));
        assert_eq!(parsed[2].branch, None);
        assert!(parsed[2].prunable);
    }

    #[test]
    fn records_keep_newlines_in_paths() {
        let parsed = records(b"worktree /x\ny\0\0");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].path, Some(&b"/x\ny"[..]));
    }

    #[test]
    fn records_of_empty_output_is_empty() {
        assert!(records(b"").is_empty());
    }

    #[test]
    fn git_env_is_local_matches_repository_variables() {
        for key in [
            "GIT_DIR",
            "git_dir",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_3",
            "GIT_QUARANTINE_PATH",
            "GIT_INTERNAL_SUPER_PREFIX",
        ] {
            assert!(git_env_is_local(key), "{key}");
        }
        for key in [
            "GIT_AUTHOR_NAME",
            "GIT_SSH_COMMAND",
            "GIT_OPTIONAL_LOCKS",
            "FOO",
        ] {
            assert!(!git_env_is_local(key), "{key}");
        }
    }
}
