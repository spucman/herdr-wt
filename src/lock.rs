use crate::{fs::open_root, quote};
use anyhow::{anyhow, bail, Context, Result};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
use fs4::fs_std::FileExt;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
};
pub fn digest(value: &OsStr) -> String {
    format!("{:x}", Sha256::digest(value.as_encoded_bytes()))
}

pub fn lock(target: &Path) -> Result<fs::File> {
    let base = lock_data_dir()
        .ok_or_else(|| anyhow!("cannot find per-user directory for target lock"))?
        .join("herdr-wt")
        .join("locks");
    if base.starts_with(target) {
        bail!("per-user lock directory would be inside target");
    }
    fs::create_dir_all(&base).context("create per-user lock directory")?;
    let lock_dir = open_root(&base)?;
    let root = open_root(target)?;
    let (device, file) = crate::fs::identity(&root)?;
    let identity = format!("{device}:{file}");
    let path = format!("{}.lock", digest(OsStr::new(&identity)));
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as _);
    }
    let file = lock_dir.open_with(path, &options)?.into_std();
    if !file.metadata()?.is_file() {
        bail!("invalid lock file");
    }
    if !FileExt::try_lock_exclusive(&file)? {
        bail!("target busy: {}", quote(target));
    }
    Ok(file)
}

fn lock_data_dir() -> Option<PathBuf> {
    // Windows runners can provide LOCALAPPDATA even when some Known Folder
    // lookups fail. The lock only needs the local data directory.
    #[cfg(windows)]
    if let Some(path) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        if path.is_absolute() {
            return Some(path);
        }
    }
    #[cfg(windows)]
    return dirs_sys::known_folder_local_app_data();
    #[cfg(target_os = "macos")]
    return dirs_sys::home_dir().map(|p| p.join("Library/Application Support"));
    #[cfg(all(unix, not(target_os = "macos")))]
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs_sys::home_dir().map(|p| p.join(".local/share")))
}
