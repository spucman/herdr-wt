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
    if raw.trim().is_empty() || raw.contains(['\\', ':', '\0']) {
        bail!("blank or unsafe path");
    }
    let mut parts = Vec::new();
    let mut glob = false;
    for s in path.split('/') {
        if s.bytes().all(|b| matches!(b, b'.' | b' ')) || protected_name(OsStr::new(s)) {
            bail!("unsafe path component");
        }
        if cfg!(windows) && !s.contains(['*', '?', '[', ']']) && windows_bad(s) {
            bail!("invalid Windows path component");
        }
        if s == "**" {
            glob = true;
            parts.push(Segment::AnyDepth);
        } else {
            if s.contains("**") {
                bail!("** must occupy a whole segment");
            }
            if s.contains(['*', '?', '[', ']']) {
                glob = true;
                validate_brackets(s)?;
            }
            parts.push(Segment::Pattern(Pattern::new(s)?));
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
) -> Result<()> {
    let mut entries = dir.entries()?.collect::<io::Result<Vec<_>>>()?;
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
        let m = dir.symlink_metadata(&name)?;
        let path = rel.join(&name);
        if fs::is_link(&m) {
            continue;
        }
        if m.is_file() && file_match {
            out.push(path);
        } else if m.is_dir() && next.iter().any(|i| *i < segments.len()) {
            next.sort_unstable();
            next.dedup();
            closure(&mut next, segments);
            let child = dir.open_dir_nofollow(&name)?;
            match child.symlink_metadata(".git") {
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            glob_walk(&child, &path, segments, &next, out)?;
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
    for (i, raw) in config.files.iter().enumerate() {
        let context = || format!("config error: files[{}] {:?}", i + 1, raw);
        let (path, glob) = selector(raw).with_context(context)?;
        if !selectors.insert(path) {
            eprintln!("warning: duplicate files entry {}", quote(Path::new(path)));
            continue;
        }
        if let Some(segments) = glob {
            let mut paths = Vec::new();
            let mut states = vec![0];
            closure(&mut states, &segments);
            glob_walk(source, Path::new(""), &segments, &states, &mut paths)
                .with_context(context)?;
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
        if e.path.ancestors().skip(1).any(|p| dirs.contains(p)) {
            println!("covered {}", quote(&e.path));
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
}
