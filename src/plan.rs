use crate::{
    checkout::Checkouts,
    config::{Config, Mode},
    fs,
    paths::{protected_name, resolve, windows_bad},
    quote,
};
use anyhow::{bail, Context, Result};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use glob::{MatchOptions, Pattern};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;
#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub source: PathBuf,
    pub dir: bool,
    literal: bool,
}

enum Segment {
    AnyDepth,
    Pattern(Pattern),
}

fn selector(raw: &str) -> Result<(&str, Option<Vec<Segment>>)> {
    let path = raw.strip_suffix('/').unwrap_or(raw);
    if raw.trim().is_empty() {
        bail!("entry is blank");
    }
    if raw.contains('\0') {
        bail!("NUL is not allowed");
    }
    if raw.contains('\\') {
        bail!("backslash is not allowed, use '/'");
    }
    if raw.contains(':') {
        bail!("':' is not allowed (drive prefix or stream name)");
    }
    if raw.starts_with('/') {
        bail!("absolute paths are not allowed");
    }
    let mut parts = Vec::new();
    let mut glob = false;
    for s in path.split('/') {
        if s.is_empty() {
            bail!("empty path segment is not allowed");
        }
        if s.bytes().all(|b| matches!(b, b'.' | b' ')) {
            bail!("{s:?} is not allowed");
        }
        if protected_name(OsStr::new(s)) {
            bail!("{s:?} is a protected name");
        }
        if cfg!(windows) && !s.contains(['*', '?', '[', ']']) && windows_bad(s) {
            bail!("{s:?} cannot be used on Windows");
        }
        if s == "**" {
            glob = true;
            parts.push(Segment::AnyDepth);
        } else {
            if s.contains("**") {
                bail!("\"**\" must be a whole path segment");
            }
            if s.contains(['*', '?', '[', ']']) {
                glob = true;
                validate_brackets(s)?;
            }
            parts.push(Segment::Pattern(
                Pattern::new(s).with_context(|| format!("invalid glob pattern {s:?}"))?,
            ));
        }
    }
    Ok((path, glob.then_some(parts)))
}

fn validate_brackets(s: &str) -> Result<()> {
    // Outside a bracket expression, a bare closing bracket is not a literal.
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ']' {
            bail!("unexpected ] in glob");
        }
        if c == '[' {
            if chars.peek() == Some(&'!') {
                chars.next();
            }
            let Some(first) = chars.next() else {
                bail!("unclosed glob class");
            };
            if first == '[' || !chars.by_ref().any(|c| c == ']') {
                bail!("invalid glob class");
            }
        }
    }
    Ok(())
}

fn matches(segment: &Segment, name: &str) -> bool {
    match segment {
        Segment::AnyDepth => true,
        Segment::Pattern(p) => p.matches_with(
            name,
            MatchOptions {
                case_sensitive: true,
                require_literal_separator: true,
                require_literal_leading_dot: false,
            },
        ),
    }
}

fn closure(states: &mut Vec<usize>, segments: &[Segment]) {
    let mut i = 0;
    while i < states.len() {
        let n = states[i];
        if matches!(segments.get(n), Some(Segment::AnyDepth)) && !states.contains(&(n + 1)) {
            states.push(n + 1);
        }
        i += 1;
    }
}

fn glob_walk(
    dir: &Dir,
    rel: &Path,
    segments: &[Segment],
    states: &[usize],
    out: &mut Vec<PathBuf>,
    links: &mut Vec<PathBuf>,
) -> Result<()> {
    let shown = if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    };
    let mut entries = dir
        .entries()
        .with_context(|| format!("read {}", quote(shown)))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("read {}", quote(shown)))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        if protected_name(&name) {
            continue;
        }
        let Some(s) = name.to_str() else {
            eprintln!(
                "warning: skipped non-Unicode glob path {}",
                quote(&rel.join(&name))
            );
            continue;
        };
        let mut next = Vec::new();
        let mut file_match = false;
        for i in states.iter().copied().filter(|&i| i < segments.len()) {
            if matches(&segments[i], s) {
                if matches!(segments[i], Segment::AnyDepth) {
                    next.push(i);
                    file_match |= i + 1 == segments.len();
                } else {
                    next.push(i + 1);
                    file_match |= i + 1 == segments.len();
                }
            }
        }
        if next.is_empty() {
            continue;
        }
        let path = rel.join(&name);
        let m = dir
            .symlink_metadata(&name)
            .with_context(|| format!("read {}", quote(&path)))?;
        if fs::is_link(&m) {
            links.push(path);
            continue;
        }
        if m.is_file() && file_match {
            out.push(path);
        } else if m.is_dir() && next.iter().any(|i| *i < segments.len()) {
            next.sort_unstable();
            next.dedup();
            closure(&mut next, segments);
            let child = dir
                .open_dir_nofollow(&name)
                .with_context(|| format!("read {}", quote(&path)))?;
            match child.symlink_metadata(".git") {
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("read {}", quote(&path.join(".git"))))
                }
            }
            glob_walk(&child, &path, segments, &next, out, links)?;
        }
    }
    Ok(())
}

enum Literal {
    Selected(Entry),
    Missing,
    Special,
}

fn literal(source: &Dir, path: &Path, absolute_root: &Path) -> Result<Literal> {
    let Some(resolved) = resolve(source, path, absolute_root)? else {
        return Ok(Literal::Missing);
    };
    let m = source.symlink_metadata(&resolved)?;
    if !m.is_file() && !m.is_dir() {
        return Ok(Literal::Special);
    }
    Ok(Literal::Selected(Entry {
        path: path.into(),
        source: resolved,
        dir: m.is_dir(),
        literal: true,
    }))
}

#[derive(Clone, Copy)]
pub struct Names {
    fold: bool,
    normalize: bool,
}

impl Names {
    pub fn for_target(target: &Dir) -> Self {
        let fold = match target.symlink_metadata(".GIT") {
            // If both spellings have distinct directory entries the volume is
            // case sensitive, regardless of the contents of the second file.
            Ok(_) => !target.entries().is_ok_and(|mut entries| {
                entries.any(|e| e.is_ok_and(|e| e.file_name() == OsStr::new(".GIT")))
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(_) => true,
        };
        Self {
            fold,
            normalize: cfg!(target_os = "macos"),
        }
    }

    pub fn key(self, p: &Path) -> Vec<OsString> {
        p.iter()
            .map(|s| match s.to_str() {
                Some(s) if self.fold || self.normalize => {
                    let mut s = if cfg!(windows) {
                        s.trim_end_matches([' ', '.']).to_owned()
                    } else {
                        s.to_owned()
                    };
                    if self.fold {
                        s = s.to_uppercase();
                    }
                    if self.normalize {
                        s = s.nfd().collect();
                    }
                    OsString::from(s)
                }
                _ => s.to_owned(),
            })
            .collect()
    }

    pub fn aliases(self, a: &Path, b: &Path) -> bool {
        self.key(a) == self.key(b)
    }

    // Recovery filenames are persistent on disk. Retain v0.1's Windows
    // folding even when selection can distinguish case-sensitive directories.
    pub fn recovery_key(mut self, path: &Path) -> PathBuf {
        self.fold |= cfg!(windows);
        self.key(path).into_iter().collect()
    }
}

fn register(names: Names, seen: &mut HashMap<Vec<OsString>, PathBuf>, path: &Path) -> Result<()> {
    // Register ancestors too: A/x and a/y still alias a directory entry.
    for path in path.ancestors().take_while(|p| !p.as_os_str().is_empty()) {
        let key = names.key(path);
        if let Some(previous) = seen.get(&key) {
            if previous != path {
                bail!("destination alias: {} and {}", quote(previous), quote(path));
            }
        } else {
            seen.insert(key, path.into());
        }
    }
    Ok(())
}

fn case_walk(
    dir: &Dir,
    src: &Path,
    dst: &Path,
    excluded: Option<&Path>,
    names: Names,
    seen: &mut HashMap<Vec<OsString>, PathBuf>,
) -> Result<()> {
    for item in dir.entries()? {
        let name = item?.file_name();
        let src_path = src.join(&name);
        if protected_name(&name) || excluded == Some(src_path.as_path()) {
            continue;
        }
        let m = dir.symlink_metadata(&name)?;
        if !fs::is_link(&m) && !m.is_file() && !m.is_dir() {
            continue;
        }
        let dst = dst.join(&name);
        register(names, seen, &dst)?;
        if m.is_dir() && !fs::is_link(&m) {
            case_walk(
                &dir.open_dir_nofollow(&name)?,
                &src_path,
                &dst,
                excluded,
                names,
                seen,
            )?;
        }
    }
    Ok(())
}

pub fn select(
    config: &Config,
    source: &Dir,
    target: &Dir,
    checkouts: &Checkouts,
) -> Result<Vec<Entry>> {
    let mut selectors = HashSet::new();
    let mut all = Vec::new();
    let mut links = std::collections::BTreeSet::new();
    for (i, raw) in config.files.iter().enumerate() {
        let context = || format!("config error: files[{}] {:?}", i + 1, raw);
        let (path, glob) = selector(raw).with_context(context)?;
        if !selectors.insert(path) {
            eprintln!("warning: duplicate files entry {}", quote(Path::new(path)));
            continue;
        }
        if let Some(segments) = glob {
            let mut paths = Vec::new();
            let mut found = Vec::new();
            let mut states = vec![0];
            closure(&mut states, &segments);
            glob_walk(
                source,
                Path::new(""),
                &segments,
                &states,
                &mut paths,
                &mut found,
            )
            .with_context(context)?;
            links.extend(found);
            if paths.is_empty() {
                eprintln!("warning: glob matched no files: {}", quote(Path::new(path)));
            }
            all.extend(paths.into_iter().map(|p| Entry {
                source: p.clone(),
                path: p,
                dir: false,
                literal: false,
            }));
        } else {
            match literal(source, Path::new(path), &checkouts.source).with_context(context)? {
                Literal::Selected(entry) => all.push(entry),
                Literal::Missing => {
                    println!("missing {} (source does not exist)", quote(Path::new(path)))
                }
                Literal::Special => {
                    eprintln!("warning: skipped special file {}", quote(Path::new(path)))
                }
            }
        }
    }
    let dirs: HashSet<_> = all
        .iter()
        .filter(|e| e.dir)
        .map(|e| e.path.clone())
        .collect();
    all.retain(|e| {
        if let Some(dir) = e.path.ancestors().skip(1).find(|p| dirs.contains(*p)) {
            println!("covered {} by {}", quote(&e.path), quote(dir));
            false
        } else {
            true
        }
    });
    let literals: HashSet<_> = all
        .iter()
        .filter(|e| e.literal)
        .map(|e| e.path.clone())
        .collect();
    let mut unique = HashSet::new();
    all.retain(|e| {
        if (!e.literal && literals.contains(&e.path)) || !unique.insert(e.path.clone()) {
            eprintln!("warning: duplicate selection {}", quote(&e.path));
            false
        } else {
            true
        }
    });
    let selected: HashSet<_> = all.iter().map(|e| e.path.clone()).collect();
    for link in &links {
        if !link.ancestors().any(|p| selected.contains(p)) {
            eprintln!(
                "warning: skipped {} (symlink matched by glob; list it literally)",
                quote(link)
            );
        }
    }
    let names = Names::for_target(target);
    let excluded = checkouts.target.strip_prefix(&checkouts.source).ok();
    let source_in_target = checkouts.source.strip_prefix(&checkouts.target).ok();
    let mut seen = HashMap::new();
    for entry in &all {
        register(names, &mut seen, &entry.path)?;
        if let Some(source_rel) = source_in_target {
            if entry.path.ancestors().any(|p| names.aliases(p, source_rel))
                || (config.overwrite
                    && source_rel
                        .ancestors()
                        .any(|p| names.aliases(p, &entry.path)))
            {
                bail!(
                    "destination overlaps the source checkout: {}",
                    quote(&entry.path)
                );
            }
        }
        if config.mode == Mode::Copy && entry.dir {
            let (parent, name) = fs::source_parent(source, &entry.source)?;
            case_walk(
                &parent.open_dir_nofollow(name)?,
                &entry.source,
                &entry.path,
                excluded,
                names,
                &mut seen,
            )
            .with_context(|| format!("inspect selected directory {}", quote(&entry.path)))?;
        }
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(raw: &str, needle: &str) {
        match selector(raw) {
            Ok(_) => panic!("accepted {raw:?}"),
            Err(e) => assert!(format!("{e:#}").contains(needle), "{raw:?}: {e:#}"),
        }
    }

    fn accepted(raw: &str, path: &str, glob: bool) {
        match selector(raw) {
            Ok((p, g)) => {
                assert_eq!(p, path, "{raw:?}");
                assert_eq!(g.is_some(), glob, "{raw:?}");
            }
            Err(e) => panic!("rejected {raw:?}: {e:#}"),
        }
    }

    #[test]
    fn selector_names_blank_nul_backslash_and_colon_rules() {
        rejected("", "entry is blank");
        rejected("   ", "entry is blank");
        rejected("x\0", "NUL is not allowed");
        rejected("a\\b", "backslash is not allowed, use '/'");
        rejected("C:/x", "':' is not allowed (drive prefix or stream name)");
        rejected("a:b", "':' is not allowed (drive prefix or stream name)");
    }

    #[test]
    fn selector_names_absolute_and_empty_segment_rules() {
        rejected("/root", "absolute paths are not allowed");
        rejected("/", "absolute paths are not allowed");
        rejected("a//", "empty path segment is not allowed");
        rejected("a//b", "empty path segment is not allowed");
    }

    #[test]
    fn selector_names_dot_and_space_only_segments() {
        for raw in [".", "./x", "..", "../x", "a/../b", "...", "a/ /b", ". "] {
            rejected(raw, "is not allowed");
        }
        rejected("a/../b", "\"..\" is not allowed");
    }

    #[test]
    fn selector_names_protected_names() {
        for raw in [
            ".git",
            ".GIT",
            "a/.gIt/x",
            ".GIT/",
            ".git.",
            ".git ",
            ".git. .",
            "a/.git../x",
            "GIT~1",
            "git~12/x",
            "a/Git~2",
            "GIT~1.",
            ".herdr-wt-tmp-x",
            ".HERDR-WT-TMP-stage",
        ] {
            rejected(raw, "is a protected name");
        }
    }

    #[test]
    fn selector_names_glob_syntax_rules() {
        rejected("some**name", "\"**\" must be a whole path segment");
        rejected("***", "\"**\" must be a whole path segment");
        rejected("bad]", "unexpected ] in glob");
        for raw in ["bad[", "[!"] {
            match selector(raw) {
                Ok(_) => panic!("accepted {raw:?}"),
                Err(e) => {
                    let msg = format!("{e:#}");
                    assert!(
                        msg.contains("unclosed") || msg.contains("invalid glob class"),
                        "{raw:?}: {msg}"
                    );
                }
            }
        }
        rejected("[[x]", "invalid glob class");
    }

    #[test]
    fn selector_accepts_valid_entries() {
        accepted(".env", ".env", false);
        accepted("a/b", "a/b", false);
        accepted("with space/x", "with space/x", false);
        accepted("**/x", "**/x", true);
        accepted("a/**/b", "a/**/b", true);
        accepted("*", "*", true);
        accepted("?", "?", true);
        accepted("[ab]x", "[ab]x", true);
        accepted(".vscode/", ".vscode", false);
        for raw in [".gitignore", ".github/x", "git~", "git~a", "my.git"] {
            accepted(raw, raw, false);
        }
    }

    #[cfg(windows)]
    #[test]
    fn selector_rejects_windows_reserved_names() {
        for raw in ["CON", "nul.txt", "a."] {
            rejected(raw, "cannot be used on Windows");
        }
    }

    #[test]
    fn case_policy_matches_the_checkout_volume() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".git"), "pointer").unwrap();
        let aliases = tmp.path().join(".GIT").exists();
        if !aliases {
            std::fs::write(tmp.path().join(".GIT"), "independent entry").unwrap();
        }
        let names = Names::for_target(&fs::open_root(tmp.path()).unwrap());
        assert_eq!(names.aliases(Path::new("Case"), Path::new("case")), aliases);
    }

    #[test]
    fn alias_detection_checks_parent_components_and_unicode() {
        let mut seen = HashMap::new();
        let names = Names {
            fold: true,
            normalize: true,
        };
        register(names, &mut seen, Path::new("Config/a")).unwrap();
        assert!(register(names, &mut seen, Path::new("config/b")).is_err());
        register(names, &mut seen, Path::new("Café")).unwrap();
        assert!(register(names, &mut seen, Path::new("Cafe\u{301}")).is_err());
    }

    #[test]
    fn case_sensitive_targets_keep_distinct_names() {
        let names = Names {
            fold: false,
            normalize: false,
        };
        let mut seen = HashMap::new();
        register(names, &mut seen, Path::new("Config/a")).unwrap();
        register(names, &mut seen, Path::new("config/b")).unwrap();
    }

    type Walked = (Vec<PathBuf>, Vec<PathBuf>);

    fn walk(root: &Path, pattern: &str) -> Result<Walked> {
        let (_, segments) = selector(pattern)?;
        let segments = segments.expect("pattern is a glob");
        let mut states = vec![0];
        closure(&mut states, &segments);
        let mut files = Vec::new();
        let mut links = Vec::new();
        glob_walk(
            &fs::open_root(root)?,
            Path::new(""),
            &segments,
            &states,
            &mut files,
            &mut links,
        )?;
        Ok((files, links))
    }

    fn slashed(paths: Vec<PathBuf>) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                let parts: Vec<_> = p.iter().map(|s| s.to_string_lossy()).collect();
                parts.join("/")
            })
            .collect()
    }

    fn try_glob(root: &Path, pattern: &str) -> Result<(Vec<String>, Vec<String>)> {
        let (files, links) = walk(root, pattern)?;
        let mut files = slashed(files);
        let mut links = slashed(links);
        files.sort();
        links.sort();
        Ok((files, links))
    }

    fn glob(root: &Path, pattern: &str) -> (Vec<String>, Vec<String>) {
        try_glob(root, pattern).unwrap_or_else(|e| panic!("{pattern:?}: {e:#}"))
    }

    fn tree(paths: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for path in paths {
            let full = tmp.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, "x").unwrap();
        }
        tmp
    }

    fn files(root: &Path, pattern: &str) -> Vec<String> {
        glob(root, pattern).0
    }

    #[test]
    fn any_depth_matches_zero_or_more_directories() {
        let tmp = tree(&["x", "a/x", "a/b/x", "a/b/y"]);
        assert_eq!(files(tmp.path(), "**/x"), ["a/b/x", "a/x", "x"]);
    }

    #[test]
    fn middle_any_depth_matches_zero_or_more_directories() {
        let tmp = tree(&["a/x", "a/b/c/x", "x", "b/x"]);
        assert_eq!(files(tmp.path(), "a/**/x"), ["a/b/c/x", "a/x"]);
    }

    #[test]
    fn single_star_does_not_cross_directories() {
        let tmp = tree(&["a.txt", "a/b.txt"]);
        assert_eq!(files(tmp.path(), "*.txt"), ["a.txt"]);
    }

    #[test]
    fn star_matches_dotfiles() {
        let tmp = tree(&[".env"]);
        assert_eq!(files(tmp.path(), "*"), [".env"]);
    }

    #[test]
    fn glob_is_case_sensitive() {
        let tmp = tree(&["a.txt"]);
        assert!(files(tmp.path(), "*.TXT").is_empty());
    }

    #[test]
    fn directories_are_never_returned() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("d")).unwrap();
        assert!(files(tmp.path(), "*").is_empty());
    }

    #[test]
    fn root_git_directory_is_skipped() {
        let tmp = tree(&[".git/config", "x"]);
        assert_eq!(files(tmp.path(), "**"), ["x"]);
    }

    #[test]
    fn nested_repository_is_not_descended() {
        let tmp = tree(&["a/.git", "a/x", "b/y"]);
        assert_eq!(files(tmp.path(), "**"), ["b/y"]);
    }

    #[test]
    fn temporary_directories_are_skipped() {
        let tmp = tree(&[".herdr-wt-tmp-x/f", "x"]);
        assert_eq!(files(tmp.path(), "**"), ["x"]);
    }

    #[test]
    fn results_are_sorted() {
        let tmp = tree(&["b", "a/c"]);
        let (found, _) = walk(tmp.path(), "**").unwrap();
        assert_eq!(slashed(found), ["a/c", "b"]);
    }

    #[cfg(unix)]
    #[test]
    fn glob_reports_matched_symlink_without_following() {
        let tmp = tree(&["real.txt"]);
        std::os::unix::fs::symlink("real.txt", tmp.path().join("link.txt")).unwrap();
        let (found, links) = glob(tmp.path(), "*.txt");
        assert_eq!(found, ["real.txt"]);
        assert_eq!(links, ["link.txt"]);
    }

    #[cfg(unix)]
    #[test]
    fn glob_reports_symlinked_dir_without_descending() {
        let tmp = tree(&["d/f"]);
        std::os::unix::fs::symlink("d", tmp.path().join("ldir")).unwrap();
        let (found, links) = glob(tmp.path(), "ldir/*");
        assert!(found.is_empty());
        assert_eq!(links, ["ldir"]);
        let (found, links) = glob(tmp.path(), "**/f");
        assert_eq!(found, ["d/f"]);
        assert_eq!(links, ["ldir"]);
    }

    #[cfg(unix)]
    #[test]
    fn glob_ignores_unmatched_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("x", tmp.path().join("link.md")).unwrap();
        let (found, links) = glob(tmp.path(), "*.txt");
        assert!(found.is_empty());
        assert!(links.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loop_terminates() {
        let tmp = tree(&["x"]);
        std::os::unix::fs::symlink(".", tmp.path().join("loop")).unwrap();
        let (found, links) = glob(tmp.path(), "**");
        assert_eq!(found, ["x"]);
        assert_eq!(links, ["loop"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_name_is_skipped_without_panic() {
        use std::os::unix::ffi::OsStrExt;
        let tmp = tree(&["ok"]);
        std::fs::write(tmp.path().join(OsStr::from_bytes(b"bad\xff")), "x").unwrap();
        assert_eq!(files(tmp.path(), "*"), ["ok"]);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_subdir_is_an_error_naming_the_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tree(&["d/locked/f"]);
        let locked = tmp.path().join("d/locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&locked).is_ok();
        let result = try_glob(tmp.path(), "d/**");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if readable {
            eprintln!("skipped: permissions are not enforced (root)");
            return;
        }
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("locked"), "{err:#}");
    }
}
