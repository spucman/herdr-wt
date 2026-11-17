use crate::{apply, checkout::Checkouts, quote};
use anyhow::{bail, Context, Result};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use glob::{MatchOptions, Pattern};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{OsStr, OsString},
    io::{self, Read},
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Copy,
    Symlink,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub overwrite: bool,
    files: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Clone)]
pub struct Entry {
    pub path: PathBuf,
    pub source: PathBuf,
    pub dir: bool,
    literal: bool,
}

pub fn load(source: &Dir, absolute_root: &Path) -> Result<Option<Config>> {
    let name = Path::new(".herdr-wt.toml");
    match source.symlink_metadata(name) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("config error: inspect .herdr-wt.toml"),
    }
    let path = resolve(source, name, absolute_root)
        .and_then(|p| p.ok_or_else(|| anyhow::anyhow!("broken config symlink")))
        .context("config error: .herdr-wt.toml must resolve inside the source checkout")?;
    validate_resolved(&path)?;
    let (parent, leaf) = apply::source_parent(source, &path)?;
    let mut contents = String::new();
    apply::regular_file(&parent, leaf)
        .context("config error: .herdr-wt.toml must resolve to a regular file")?
        .read_to_string(&mut contents)
        .context("config error: read .herdr-wt.toml")?;
    let config: Config = toml::from_str(&contents).context("config error: invalid TOML")?;
    if cfg!(windows) && config.mode == Mode::Symlink {
        bail!("config error: Windows supports copy mode only");
    }
    if config
        .commands
        .iter()
        .any(|c| c.trim().is_empty() || c.contains('\0'))
    {
        bail!("config error: commands must not be blank or contain NUL");
    }
    if config.files.is_empty() && config.commands.is_empty() {
        eprintln!("warning: empty config");
    }
    Ok(Some(config))
}

pub fn git_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|s| {
        let s = s.trim_end_matches(['.', ' ']);
        s.eq_ignore_ascii_case(".git")
            || s.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("git~"))
                && s.get(4..)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    })
}

pub fn protected_name(name: &OsStr) -> bool {
    git_name(name)
        || name.to_str().is_some_and(|s| {
            s.get(..14)
                .is_some_and(|p| p.eq_ignore_ascii_case(".herdr-wt-tmp-"))
        })
}

fn validate_resolved(path: &Path) -> Result<()> {
    for c in path.components() {
        if !matches!(c, std::path::Component::Normal(n) if !protected_name(n)) {
            bail!("config error: resolved path is protected: {}", quote(path));
        }
    }
    if path.as_os_str().is_empty() {
        bail!("config error: selected path resolves to the source root");
    }
    Ok(())
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

fn windows_bad(s: &str) -> bool {
    let stem = s
        .split('.')
        .next()
        .unwrap_or(s)
        .trim_end()
        .to_ascii_uppercase();
    s.ends_with([' ', '.'])
        || s.chars()
            .any(|c| c.is_control() || "<>\"|:*?\\/".contains(c))
        || matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        )
        || (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.get(3..).is_some_and(|tail| {
                matches!(
                    tail,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
}

pub fn check_native_destination(name: &OsStr) -> Result<()> {
    if cfg!(windows) && name.to_str().is_some_and(windows_bad) {
        bail!(
            "filename cannot be reproduced on Windows: {}",
            quote(Path::new(name))
        );
    }
    Ok(())
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
        if apply::is_link(&m) {
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

fn resolve(source: &Dir, path: &Path, absolute_root: &Path) -> Result<Option<PathBuf>> {
    use std::path::Component;
    let mut remaining: VecDeque<OsString> =
        path.components().map(|c| c.as_os_str().into()).collect();
    let mut resolved = PathBuf::new();
    let mut links = 0;
    while let Some(name) = remaining.pop_front() {
        let name = Path::new(&name);
        match name.components().next() {
            Some(Component::CurDir) => continue,
            Some(Component::ParentDir) => {
                if !resolved.pop() {
                    bail!("resolves outside the source checkout");
                }
                continue;
            }
            Some(Component::Normal(n)) if !protected_name(n) => {}
            _ => bail!("resolves to an unsafe or protected path"),
        }
        let candidate = resolved.join(name);
        let (parent, leaf) = apply::source_parent(source, &candidate)?;
        let m = match parent.symlink_metadata(leaf) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if apply::is_link(&m) {
            links += 1;
            if links > 40 {
                bail!("too many source symlinks");
            }
            let mut link = parent.read_link_contents(leaf)?;
            if link.is_absolute() {
                // Windows link targets can use an ordinary drive prefix
                // while std::fs::canonicalize returns a verbatim prefix.
                link = relative_absolute_link(&link, absolute_root)?;
                resolved.clear();
            } else if link.has_root() {
                bail!("resolves outside the source checkout");
            }
            for c in link.components().rev() {
                remaining.push_front(c.as_os_str().into());
            }
        } else {
            if !remaining.is_empty() && !m.is_dir() {
                return Ok(None);
            }
            resolved = candidate;
        }
    }
    validate_resolved(&resolved)?;
    Ok(Some(resolved))
}

fn relative_absolute_link(link: &Path, root: &Path) -> Result<PathBuf> {
    if let Ok(rel) = link.strip_prefix(root) {
        return Ok(rel.into());
    }
    // Resolve only the longest existing directory prefix, retaining the
    // unresolved suffix for the component-by-component capability walk.
    for ancestor in link.ancestors().skip(1) {
        if let Ok(canon) = dunce::canonicalize(ancestor) {
            if let Ok(relative) = canon.strip_prefix(root) {
                return Ok(relative.join(link.strip_prefix(ancestor)?));
            }
        }
    }
    bail!("resolves outside the source checkout")
}

fn literal(source: &Dir, path: &Path, absolute_root: &Path) -> Result<Option<Entry>> {
    let Some(resolved) = resolve(source, path, absolute_root)? else {
        return Ok(None);
    };
    let m = source.symlink_metadata(&resolved)?;
    if !m.is_file() && !m.is_dir() {
        eprintln!("warning: skipped special file {}", quote(path));
        return Ok(None);
    }
    Ok(Some(Entry {
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
        Self {
            fold: target.symlink_metadata(".GIT").is_ok() || cfg!(windows),
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
        if !apply::is_link(&m) && !m.is_file() && !m.is_dir() {
            continue;
        }
        let dst = dst.join(&name);
        register(names, seen, &dst)?;
        if m.is_dir() && !apply::is_link(&m) {
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
        } else if let Some(entry) =
            literal(source, Path::new(path), &checkouts.source).with_context(context)?
        {
            all.push(entry);
        } else {
            println!("missing {} (source does not exist)", quote(Path::new(path)));
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
        if config.overwrite
            && source_in_target
                .is_some_and(|p| p.ancestors().any(|p| names.aliases(p, &entry.path)))
        {
            bail!(
                "replacement would remove the source checkout: {}",
                quote(&entry.path)
            );
        }
        if config.mode == Mode::Copy && entry.dir {
            let (parent, name) = apply::source_parent(source, &entry.source)?;
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
