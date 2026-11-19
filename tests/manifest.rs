use std::collections::HashSet;

use toml::{Table, Value};

const MANIFEST: &str = include_str!("../herdr-plugin.toml");
const README: &str = include_str!("../README.md");
const CARGO: &str = include_str!("../Cargo.toml");

fn parse(text: &str) -> Table {
    toml::from_str::<Table>(text).unwrap()
}

fn manifest() -> Table {
    parse(MANIFEST)
}

fn entries<'a>(manifest: &'a Table, section: &str) -> Vec<&'a Table> {
    manifest
        .get(section)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing [[{section}]]"))
        .iter()
        .map(|v| v.as_table().unwrap())
        .collect()
}

fn strings(entry: &Table, key: &str) -> Vec<String> {
    entry
        .get(key)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing {key} in {entry:?}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}

fn command(entry: &Table) -> Vec<String> {
    strings(entry, "command")
}

fn readme_minimum(prefix: &str) -> String {
    README
        .match_indices(prefix)
        .map(|(at, _)| &README[at + prefix.len()..])
        .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        .find_map(|rest| rest.split_once('+').map(|(version, _)| version.to_owned()))
        .unwrap_or_else(|| panic!("README has no `{prefix}<version>+` phrase"))
}

#[test]
fn manifest_id_is_herdr_wt() {
    assert_eq!(manifest()["id"].as_str(), Some("herdr-wt"));
}

#[test]
fn readme_documents_min_herdr_version() {
    let manifest = manifest();
    let min = manifest["min_herdr_version"].as_str().unwrap();
    assert!(
        README.contains(&format!("Herdr {min}+")),
        "README does not contain `Herdr {min}+`"
    );
}

#[test]
fn worktree_created_events_end_with_event_subcommand() {
    let manifest = manifest();
    let events: Vec<_> = entries(&manifest, "events")
        .into_iter()
        .filter(|e| e["on"].as_str() == Some("worktree.created"))
        .collect();
    assert!(!events.is_empty());
    for event in events {
        assert_eq!(command(event).last().map(String::as_str), Some("event"));
    }
}

#[test]
fn actions_end_with_action_subcommand_and_use_workspace_context() {
    let manifest = manifest();
    let actions = entries(&manifest, "actions");
    assert!(!actions.is_empty());
    for action in actions {
        assert_eq!(command(action).last().map(String::as_str), Some("action"));
        assert_eq!(strings(action, "contexts"), ["workspace"]);
    }
}

#[test]
fn action_ids_are_unique() {
    let manifest = manifest();
    let mut seen = HashSet::new();
    for action in entries(&manifest, "actions") {
        let id = action["id"].as_str().unwrap();
        assert!(seen.insert(id), "duplicate action id {id}");
    }
}

#[test]
fn only_windows_entries_use_exe_executables() {
    let manifest = manifest();
    for section in ["build", "events", "actions"] {
        for entry in entries(&manifest, section) {
            let windows = strings(entry, "platforms").iter().any(|p| p == "windows");
            let executable = command(entry).remove(0);
            assert_eq!(
                executable.ends_with(".exe"),
                windows,
                "[[{section}]] executable {executable} for platforms {:?}",
                entry["platforms"]
            );
        }
    }
}

#[test]
fn cargo_rust_version_matches_readme_minimum() {
    let cargo = parse(CARGO);
    let rust_version = cargo["package"]["rust-version"].as_str().unwrap();
    assert_eq!(rust_version, readme_minimum("Rust "));
}
