#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
};

mod common;
use common::*;

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o777
}

fn root_entries_with_prefix(target: &Path, prefix: &str) -> Vec<String> {
    fs::read_dir(target)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(prefix))
        .collect()
}

fn assert_no_prefix(target: &Path, prefix: &str) {
    let found = root_entries_with_prefix(target, prefix);
    assert!(found.is_empty(), "unexpected {prefix}* entries: {found:?}");
}

fn restore_tree(root: &Path) {
    let Ok(meta) = fs::symlink_metadata(root) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    if mode(root) & 0o200 == 0 {
        set_mode(root, 0o755);
    }
    for entry in fs::read_dir(root).unwrap() {
        restore_tree(&entry.unwrap().path());
    }
}

fn assert_read_only_dir_copied(config: &str, ro_dir: &str) {
    let r = Repo::new();
    r.config(config);
    r.file(&format!("{ro_dir}/f"), "content");
    let ro_source = r.source.join(ro_dir);
    let ro_target = r.target.join(ro_dir);
    set_mode(&ro_source, 0o500);

    let got = r.run(&[]);

    let target_mode = ro_target.exists().then(|| mode(&ro_target));
    restore_tree(r.base.path());
    status(&got, 0);
    assert!(ro_target.join("f").is_file());
    assert_eq!(target_mode, Some(0o500));
    assert_no_prefix(&r.target, ".herdr-wt-tmp-");
}

#[test]
fn read_only_source_dir_is_copied() {
    if is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    assert_read_only_dir_copied("files = ['ro']", "ro");
}

#[test]
fn nested_read_only_dir_is_copied() {
    if is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    assert_read_only_dir_copied("files = ['tree']", "tree/ro");
}

#[test]
fn overwrite_of_tree_with_read_only_subdir_leaves_no_backup() {
    if is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    let r = Repo::new();
    r.config("overwrite = true\nfiles = ['tree']");
    r.file("tree/f", "one");
    r.file("tree/ro/g", "content");
    let ro_source = r.source.join("tree/ro");
    set_mode(&ro_source, 0o500);

    let first = r.run(&[]);
    fs::write(r.source.join("tree/f"), "two").unwrap();
    let second = r.run(&[]);
    let third = r.run(&[]);

    restore_tree(r.base.path());
    status(&first, 0);
    status(&second, 0);
    assert!(out(&second).contains("replaced"), "{}", out(&second));
    assert_no_prefix(&r.target, ".herdr-wt-tmp-original-");
    status(&third, 0);
    assert!(
        !err(&third).contains("displaced original"),
        "{}",
        err(&third)
    );
}

#[test]
fn failed_copy_with_read_only_subdir_leaves_no_stage() {
    if is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    let r = Repo::new();
    r.config("files = ['tree']");
    r.file("tree/a/f", "content");
    r.file("tree/b/f", "secret");
    // copy_dir visits entries in raw readdir order, which is filesystem
    // dependent. Read that order back and let the first directory become
    // read-only (its mode is applied once its copy completes) and the second
    // fail on an unreadable file, so the failure always follows a staged
    // read-only directory and the cleanup must cope with it.
    let names: Vec<_> = fs::read_dir(r.source.join("tree"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let (ro_source, unreadable) = (
        r.source.join("tree").join(&names[0]),
        r.source.join("tree").join(&names[1]).join("f"),
    );
    set_mode(&ro_source, 0o500);
    set_mode(&unreadable, 0o000);

    let got = r.run(&[]);

    set_mode(&unreadable, 0o644);
    restore_tree(r.base.path());
    status(&got, 2);
    assert!(
        err(&got).contains("failed"),
        "expected a copy failure: {}",
        err(&got)
    );
    assert_no_prefix(&r.target, ".herdr-wt-tmp-stage-");
}
