use crate::{
    fs::{inspect, is_link, mkdir},
    quote,
};
use anyhow::{bail, Context, Result};
use cap_fs_ext::DirExt;
use cap_std::fs::Dir;
use std::{ffi::OsStr, io, path::Path};
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
            eprintln!(
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
}
