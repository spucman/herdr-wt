use crate::{
    fs::{inspect, is_link, mkdir},
    quote,
};
use anyhow::{bail, Context, Result};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use std::{
    ffi::OsStr,
    io::{self, Write},
    path::Path,
};
const STAGING: &str = ".herdr-wt-tmp-stage-";
pub const BACKUP: &str = ".herdr-wt-tmp-original-";

/// Never dispose of a previous run's recovery data along with its parent.
/// Fail closed on unreadable entries; do not descend through links/junctions.
pub fn ensure_recoverable(dir: &Dir, path: &Path) -> Result<()> {
    for item in dir
        .entries()
        .with_context(|| format!("inspect {} for recovery records", quote(path)))?
    {
        let name = item?.file_name();
        let child = path.join(&name);
        if name
            .to_str()
            .and_then(|s| s.get(..BACKUP.len()))
            .is_some_and(|s| s.eq_ignore_ascii_case(BACKUP))
        {
            bail!(
                "displaced original: inspect {} before replacing its parent",
                quote(&child)
            );
        }
        let m = dir.symlink_metadata(&name)?;
        if !is_link(&m) && m.is_dir() {
            ensure_recoverable(&dir.open_dir_nofollow(&name)?, &child)?;
        }
    }
    Ok(())
}

/// Remember the first error of a best-effort walk without stopping it.
#[cfg(unix)]
fn keep_first(first: &mut io::Result<()>, next: io::Result<()>) {
    if first.is_ok() {
        *first = next;
    }
}

/// Add owner rwx to the directory `name` below `parent` without following links.
#[cfg(unix)]
fn grant_owner_rwx(parent: &Dir, name: &OsStr) -> io::Result<()> {
    use rustix::{
        fs::{fchmod, fstat, openat, Mode, OFlags},
        io::Errno,
    };

    // Capability directories may use O_PATH on Linux, which fchmod rejects.
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    match openat(parent, name, OFlags::RDONLY | flags, Mode::empty()) {
        Ok(handle) => {
            let mode = fstat(&handle)?.st_mode & 0o777;
            Ok(fchmod(&handle, Mode::from_raw_mode(mode | 0o700))?)
        }
        Err(Errno::ACCESS) => grant_unreadable(parent, name, flags),
        Err(e) => Err(e.into()),
    }
}

/// A directory without the read bit cannot be opened for `fchmod`; chmod the
/// inode behind an `O_PATH` handle instead, which never resolves a link.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn grant_unreadable(parent: &Dir, name: &OsStr, flags: rustix::fs::OFlags) -> io::Result<()> {
    use rustix::fs::{chmod, fstat, openat, Mode, OFlags};
    use std::os::fd::AsRawFd;

    let handle = openat(parent, name, OFlags::PATH | flags, Mode::empty())?;
    let mode = fstat(&handle)?.st_mode & 0o777;
    let proc_path = format!("/proc/self/fd/{}", handle.as_raw_fd());
    Ok(chmod(proc_path, Mode::from_raw_mode(mode | 0o700))?)
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn grant_unreadable(_: &Dir, _: &OsStr, _: rustix::fs::OFlags) -> io::Result<()> {
    Err(io::ErrorKind::PermissionDenied.into())
}

/// Best-effort: restore owner rwx on the read-only tree at `name` so it can be
/// deleted. Links are never followed. A failure on one entry never prevents
/// its siblings from being fixed; the first error is returned at the end.
#[cfg(unix)]
fn make_writable(parent: &Dir, name: &OsStr) -> io::Result<()> {
    let mut result = grant_owner_rwx(parent, name);
    let dir = match parent.open_dir_nofollow(name) {
        Ok(dir) => dir,
        Err(e) => {
            keep_first(&mut result, Err(e));
            return result;
        }
    };
    let entries = match dir.entries() {
        Ok(entries) => entries,
        Err(e) => {
            keep_first(&mut result, Err(e));
            return result;
        }
    };
    for item in entries {
        let child = match item {
            Ok(item) => item.file_name(),
            Err(e) => {
                keep_first(&mut result, Err(e));
                continue;
            }
        };
        match dir.symlink_metadata(&child) {
            Ok(m) if !is_link(&m) && m.is_dir() => {
                keep_first(&mut result, make_writable(&dir, &child));
            }
            Ok(_) => {}
            Err(e) => keep_first(&mut result, Err(e)),
        }
    }
    result
}

fn remove(dir: &Dir, entry: &OsStr) -> io::Result<()> {
    let Some(m) = inspect(dir, entry)? else {
        return Ok(());
    };
    if is_link(&m) {
        #[cfg(windows)]
        return dir.remove_file_or_symlink(entry);
        #[cfg(not(windows))]
        return dir.remove_file(entry);
    }
    if m.is_dir() {
        // Only trees this module created may be made writable; never a user directory.
        #[cfg(unix)]
        if entry
            .to_str()
            .is_some_and(|s| s.starts_with(STAGING) || s.starts_with(BACKUP))
        {
            let _ = make_writable(dir, entry);
        }
        dir.remove_dir_all(entry)
    } else {
        dir.remove_file(entry)
    }
}

pub struct Stage<'a> {
    parent: &'a Dir,
    name: String,
    pub dir: Dir,
}

impl<'a> Stage<'a> {
    pub fn new(parent: &'a Dir) -> Result<Self> {
        for _ in 0..32 {
            let name = format!("{STAGING}{:016x}", fastrand::u64(..));
            match mkdir(parent, OsStr::new(&name)) {
                Ok(()) => {
                    return Ok(Self {
                        parent,
                        dir: parent.open_dir_nofollow(&name)?,
                        name,
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            }
        }
        bail!("cannot allocate staging directory")
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if let Err(e) = remove(self.parent, OsStr::new(&self.name)) {
            // Not eprintln!: it panics on a broken stderr, fatal inside drop.
            let _ = writeln!(
                io::stderr(),
                "warning: could not remove staging directory {}: {e}",
                self.name
            );
        }
    }
}

pub fn swap(from: &Dir, payload: &OsStr, dst: &Dir, name: &OsStr, backup: &OsStr) -> Result<()> {
    mkdir(dst, backup)?;
    let original = OsStr::new("original");
    let backup_dir = dst.open_dir_nofollow(backup)?;
    if let Err(e) = dst.rename(name, &backup_dir, original) {
        let _ = dst.remove_dir(backup);
        return Err(e).context("move old destination aside");
    }
    // Inspect the object we actually displaced as well: the destination
    // could have changed between preview and the rename.
    let publish = || -> Result<()> {
        let m = backup_dir.symlink_metadata(original)?;
        if m.is_dir() && !is_link(&m) {
            ensure_recoverable(&backup_dir.open_dir_nofollow(original)?, Path::new(name))?;
        }
        Ok(rename_new(from, payload, dst, name)?)
    };
    if let Err(e) = publish() {
        let restored = match inspect(dst, name) {
            Ok(None) => rename_new(&backup_dir, original, dst, name).is_ok(),
            _ => false,
        };
        drop(backup_dir);
        if restored {
            let _ = dst.remove_dir(backup);
        } else {
            eprintln!(
                "warning: could not restore displaced original at {}/original",
                quote(Path::new(backup))
            );
        }
        return Err(e).context("install staged replacement");
    }
    drop(backup_dir);
    if let Err(e) = remove(dst, backup) {
        eprintln!("warning: replacement installed; could not remove displaced original at {}/original: {e}", quote(Path::new(backup)));
    }
    Ok(())
}

#[cfg(unix)]
pub fn rename_new(from: &Dir, payload: &OsStr, dst: &Dir, name: &OsStr) -> io::Result<()> {
    rustix::fs::renameat_with(from, payload, dst, name, rustix::fs::RenameFlags::NOREPLACE)
        .map_err(Into::into)
}

#[cfg(windows)]
pub fn rename_new(from: &Dir, payload: &OsStr, dst: &Dir, name: &OsStr) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    fn wide_child(dir: &Dir, leaf: &OsStr) -> io::Result<Vec<u16>> {
        if Path::new(leaf).file_name() != Some(leaf) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let p = winx::file::get_file_path(&dir.try_clone()?.into_std_file())?.join(leaf);
        Ok(p.as_os_str().encode_wide().chain(Some(0)).collect())
    }
    let from_name = wide_child(from, payload)?;
    let to_name = wide_child(dst, name)?;
    // SAFETY: both vectors remain live and NUL-terminated for the call.
    // No MOVEFILE_REPLACE_EXISTING (or COPY_ALLOWED): one same-volume move.
    if unsafe { MoveFileExW(from_name.as_ptr(), to_name.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::open_root;
    use std::fs;

    #[test]
    fn publication_and_restore_never_replace_a_new_arrival() {
        let temp = tempfile::tempdir().unwrap();
        let dir = open_root(temp.path()).unwrap();
        fs::write(temp.path().join("destination"), "new arrival").unwrap();
        for payload in ["payload", "original"] {
            fs::write(temp.path().join(payload), "kept").unwrap();
            assert!(
                rename_new(&dir, OsStr::new(payload), &dir, OsStr::new("destination")).is_err()
            );
            assert_eq!(dir.read_to_string(payload).unwrap(), "kept");
            assert_eq!(dir.read_to_string("destination").unwrap(), "new arrival");
        }
    }

    #[test]
    fn swap_restores_original_if_publish_fails() {
        let temp = tempfile::tempdir().unwrap();
        let dir = open_root(temp.path()).unwrap();
        let old = OsStr::new("old");
        let backup = OsStr::new("backup");
        mkdir(&dir, old).unwrap();
        fs::write(temp.path().join(old).join("kept"), "old contents").unwrap();
        assert!(swap(&dir, OsStr::new("missing"), &dir, old, backup).is_err());
        assert_eq!(
            fs::read_to_string(temp.path().join(old).join("kept")).unwrap(),
            "old contents"
        );
        assert!(!temp.path().join(backup).exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_never_chmods_unprefixed_entries() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let user = temp.path().join("user");
        fs::create_dir(&user).unwrap();
        fs::write(user.join("x"), "kept").unwrap();
        fs::set_permissions(&user, fs::Permissions::from_mode(0o500)).unwrap();
        if fs::File::create(user.join("probe")).is_ok() {
            eprintln!("skipped: running as root");
            return;
        }
        let dir = open_root(temp.path()).unwrap();

        let result = remove(&dir, OsStr::new("user"));

        let mode = fs::metadata(&user).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(&user, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.is_err());
        assert_eq!(mode, 0o500);
    }

    #[cfg(unix)]
    #[test]
    fn remove_makes_prefixed_read_only_trees_writable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dir = open_root(temp.path()).unwrap();
        for prefix in [STAGING, BACKUP] {
            let name = format!("{prefix}test");
            let sub = temp.path().join(&name).join("a").join("b");
            fs::create_dir_all(&sub).unwrap();
            fs::write(sub.join("f"), "x").unwrap();
            for p in [sub.clone(), sub.parent().unwrap().to_path_buf()] {
                fs::set_permissions(&p, fs::Permissions::from_mode(0o500)).unwrap();
            }
            fs::set_permissions(temp.path().join(&name), fs::Permissions::from_mode(0o500))
                .unwrap();

            remove(&dir, OsStr::new(&name)).unwrap();

            assert!(!temp.path().join(&name).exists());
        }
    }

    /// Best-effort test cleanup: make every directory under `path` traversable again.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn restore_modes(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        if fs::set_permissions(path, fs::Permissions::from_mode(0o700)).is_err() {
            return;
        }
        for item in fs::read_dir(path).into_iter().flatten().flatten() {
            if item.file_type().is_ok_and(|t| t.is_dir()) {
                restore_modes(&item.path());
            }
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn remove_handles_unreadable_siblings_among_read_only_directories() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let probe = temp.path().join("probe");
        fs::create_dir(&probe).unwrap();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o500)).unwrap();
        let root = fs::File::create(probe.join("probe")).is_ok();
        fs::set_permissions(&probe, fs::Permissions::from_mode(0o700)).unwrap();
        if root {
            eprintln!("skipped: running as root");
            return;
        }
        let dir = open_root(temp.path()).unwrap();
        let name = format!("{BACKUP}siblings");
        let tree = temp.path().join(&name);
        // (directory, mode) per sibling; "inner" nests below the unreadable one.
        let siblings = [
            ("a", 0o500),
            ("b", 0o000),
            ("c", 0o500),
            ("d", 0o300),
            ("e", 0o500),
        ];
        for (sibling, _) in siblings {
            let inner = tree.join(sibling).join("inner");
            fs::create_dir_all(&inner).unwrap();
            fs::write(inner.join("f"), "x").unwrap();
            fs::set_permissions(&inner, fs::Permissions::from_mode(0o500)).unwrap();
        }
        for (sibling, mode) in siblings {
            fs::set_permissions(tree.join(sibling), fs::Permissions::from_mode(mode)).unwrap();
        }
        fs::set_permissions(&tree, fs::Permissions::from_mode(0o500)).unwrap();

        let result = remove(&dir, OsStr::new(&name));

        let leftover = tree.exists();
        restore_modes(&tree);
        result.unwrap();
        assert!(!leftover);
    }

    #[cfg(unix)]
    #[test]
    fn make_writable_never_follows_links() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o500)).unwrap();
        let name = format!("{STAGING}links");
        let tree = temp.path().join(&name);
        fs::create_dir(&tree).unwrap();
        symlink(&outside, tree.join("link")).unwrap();
        let dir = open_root(temp.path()).unwrap();

        let _ = make_writable(&dir, OsStr::new(&name));

        let mode = fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(mode, 0o500);
    }
}
