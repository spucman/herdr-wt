use crate::quote;
use anyhow::{anyhow, bail, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, DirBuilder, File, Metadata, OpenOptions},
};
use std::{ffi::OsStr, io, path::Path};
pub fn open_root(path: &Path) -> io::Result<Dir> {
    Dir::open_ambient_dir(path, ambient_authority())
}

/// Directory identity for checkout comparisons and the existing lock-key format.
pub fn identity(dir: &Dir) -> io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let m = dir.dir_metadata()?;
        Ok((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        let m = winapi_util::file::information(dir.try_clone()?.into_std_file())?;
        Ok((m.volume_serial_number(), m.file_index()))
    }
}

pub fn is_link(m: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use cap_fs_ext::OsMetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    m.file_type().is_symlink()
}

pub fn inspect(dir: &Dir, name: &OsStr) -> io::Result<Option<Metadata>> {
    match dir.symlink_metadata(name) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn mkdir(dir: &Dir, name: &OsStr) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    #[cfg(unix)]
    {
        use cap_std::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    #[cfg(windows)]
    builder.recursive(false);
    dir.create_dir_with(name, &builder)
}

pub fn parent<'a>(root: &Dir, path: &'a Path, create: bool) -> Result<Option<(Dir, &'a OsStr)>> {
    let leaf = path
        .file_name()
        .ok_or_else(|| anyhow!("missing filename"))?;
    let mut dir = root.try_clone()?;
    for part in path.parent().unwrap_or(Path::new("")).components() {
        let std::path::Component::Normal(name) = part else {
            bail!("unsafe parent path");
        };
        match inspect(&dir, name)? {
            Some(m) if is_link(&m) || !m.is_dir() => bail!(
                "destination/source parent is a link or not a directory: {}",
                quote(Path::new(name))
            ),
            Some(_) => {}
            None if !create => return Ok(None),
            None => match mkdir(&dir, name) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.into()),
            },
        }
        dir = dir.open_dir_nofollow(name)?;
    }
    Ok(Some((dir, leaf)))
}

pub fn source_parent<'a>(root: &Dir, path: &'a Path) -> Result<(Dir, &'a OsStr)> {
    parent(root, path, false)?.ok_or_else(|| anyhow!("source parent does not exist"))
}

pub fn regular_file(parent: &Dir, leaf: &OsStr) -> io::Result<File> {
    let m = parent.symlink_metadata(leaf)?;
    if is_link(&m) || !m.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as _);
    }
    let file = parent.open_with(leaf, &options)?;
    let m = file.metadata()?;
    if is_link(&m) || !m.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

pub fn create_file(dir: &Dir, name: &OsStr) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    dir.open_with(name, &options)
}
