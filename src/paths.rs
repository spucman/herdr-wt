use crate::{fs, quote};
use anyhow::{bail, Result};
use cap_std::fs::Dir;
use std::{
    collections::VecDeque,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
};
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

pub fn validate_resolved(path: &Path) -> Result<()> {
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

pub fn windows_bad(s: &str) -> bool {
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

pub fn resolve(source: &Dir, path: &Path, absolute_root: &Path) -> Result<Option<PathBuf>> {
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
        let (parent, leaf) = fs::source_parent(source, &candidate)?;
        let m = match parent.symlink_metadata(leaf) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if fs::is_link(&m) {
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
