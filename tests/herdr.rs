mod common;
use common::*;
use serde_json::{json, Value};
use std::{fs, path::Path, process::Output};

fn fixture(name: &str, source: &Path, target: &Path) -> Value {
    let raw = match name {
        "worktree_created.json" => include_str!("fixtures/worktree_created.json"),
        "action_context.json" => include_str!("fixtures/action_context.json"),
        other => panic!("unknown fixture {other}"),
    };
    let mut value: Value = serde_json::from_str(raw).unwrap();
    let paths: Vec<(&str, &Path)> = if name == "worktree_created.json" {
        vec![
            ("/data/worktree/path", target),
            ("/data/workspace/worktree/checkout_path", target),
            ("/data/workspace/worktree/repo_root", source),
        ]
    } else {
        vec![
            ("/worktree/checkout_path", target),
            ("/worktree/repo_root", source),
            ("/workspace_cwd", target),
            ("/focused_pane_cwd", target),
        ]
    };
    for (pointer, path) in paths {
        *value.pointer_mut(pointer).unwrap() = json!(path);
    }
    let key = if name == "worktree_created.json" {
        "/data/workspace/worktree/repo_key"
    } else {
        "/worktree/repo_key"
    };
    *value.pointer_mut(key).unwrap() = json!(source.join(".git"));
    value
}

fn event_fixture(r: &Repo) -> Value {
    fixture("worktree_created.json", &r.source, &r.target)
}

fn action_fixture(r: &Repo) -> Value {
    fixture("action_context.json", &r.source, &r.target)
}

fn configured() -> Repo {
    let r = Repo::new();
    r.config("files = [\"sample\"]");
    r.file("sample", "value");
    r
}

fn run_event(r: &Repo, payload: Option<&str>) -> Output {
    let mut command = r.exe();
    command.arg("event");
    if let Some(payload) = payload {
        command.env("HERDR_PLUGIN_EVENT_JSON", payload);
    }
    command.output().unwrap()
}

fn run_action(r: &Repo, context: Option<&str>) -> Output {
    let mut command = r.exe();
    command.arg("action");
    if let Some(context) = context {
        command.env("HERDR_PLUGIN_CONTEXT_JSON", context);
    }
    command.output().unwrap()
}

fn assert_copied(r: &Repo) {
    assert_eq!(
        fs::read_to_string(r.target.join("sample")).unwrap(),
        "value"
    );
}

#[test]
fn real_event_payload_initializes_worktree() {
    let r = configured();
    let o = run_event(&r, Some(&event_fixture(&r).to_string()));
    status(&o, 0);
    assert_copied(&r);
}

#[test]
fn real_action_context_initializes_worktree() {
    let r = configured();
    let o = run_action(&r, Some(&action_fixture(&r).to_string()));
    status(&o, 0);
    assert_copied(&r);
}

#[test]
fn event_falls_back_to_workspace_checkout_path() {
    let r = configured();
    let mut payload = event_fixture(&r);
    payload["data"].as_object_mut().unwrap().remove("worktree");
    status(&run_event(&r, Some(&payload.to_string())), 0);
    assert_copied(&r);
}

#[test]
fn event_with_null_or_empty_primary_uses_fallback() {
    let r = configured();
    for primary in [Value::Null, json!("")] {
        let mut payload = event_fixture(&r);
        *payload.pointer_mut("/data/worktree/path").unwrap() = primary;
        status(&run_event(&r, Some(&payload.to_string())), 0);
        assert_copied(&r);
        fs::remove_file(r.target.join("sample")).unwrap();
    }
}

#[test]
fn missing_payload_exits_1() {
    let r = configured();
    let o = run_event(&r, None);
    status(&o, 1);
    assert!(
        err(&o).contains("missing Herdr event payload"),
        "{}",
        err(&o)
    );
    let o = run_action(&r, None);
    status(&o, 1);
    assert!(
        err(&o).contains("missing Herdr action context"),
        "{}",
        err(&o)
    );
    assert!(!r.target.join("sample").exists());
}

#[test]
fn invalid_json_exits_1() {
    let r = configured();
    status(&run_event(&r, Some("{")), 1);
    assert!(!r.target.join("sample").exists());
}

#[test]
fn wrong_event_name_exits_1() {
    let r = configured();
    let mut payload = event_fixture(&r);
    payload["event"] = json!("worktree.removed");
    let o = run_event(&r, Some(&payload.to_string()));
    status(&o, 1);
    assert!(
        err(&o).contains("expected worktree.created event, got \"worktree.removed\""),
        "{}",
        err(&o)
    );
    assert!(!r.target.join("sample").exists());
}

#[test]
fn relative_path_exits_1() {
    let r = configured();
    let mut payload = event_fixture(&r);
    *payload.pointer_mut("/data/worktree/path").unwrap() = json!("relative/target");
    let o = run_event(&r, Some(&payload.to_string()));
    status(&o, 1);
    assert!(err(&o).contains("invalid or lossy"), "{}", err(&o));
    assert!(!r.target.join("sample").exists());
}

#[test]
fn action_with_null_worktree_exits_1() {
    let r = configured();
    let mut context = action_fixture(&r);
    context["worktree"] = Value::Null;
    let o = run_action(&r, Some(&context.to_string()));
    status(&o, 1);
    assert!(err(&o).contains("no recorded worktree"), "{}", err(&o));
    assert!(!r.target.join("sample").exists());
}

#[test]
fn event_payload_naming_source_is_refused_with_4() {
    let r = configured();
    let payload = fixture("worktree_created.json", &r.source, &r.source);
    status(&run_event(&r, Some(&payload.to_string())), 4);
    assert!(!r.target.join("sample").exists());
}

#[test]
fn event_without_config_is_silent() {
    let r = Repo::new();
    let o = run_event(&r, Some(&event_fixture(&r).to_string()));
    status(&o, 0);
    assert_eq!(out(&o), "");
}
