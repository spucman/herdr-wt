use crate::{
    checkout::Checkouts,
    config::{Config, Mode},
    fs::*,
    lock::digest,
    paths,
    plan::{Entry, Names},
    quote,
    replace::{self, rename_new, swap, Stage, BACKUP},
};
use anyhow::{anyhow, bail, Context, Result};
use cap_fs_ext::DirExt;
use cap_std::fs::{Dir, Metadata};
use std::{ffi::OsStr, io, path::Path};
fn link(dir: &Dir, name: &OsStr, target: &Path, m: &Metadata) -> Result<()> {
    #[cfg(unix)]
    {
        let _ = m;
        dir.symlink_contents(target, name)?;
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::OsMetadataExt;
        if !m.file_type().is_symlink() {
            bail!("cannot recreate unsupported Windows reparse point");
        }
        if m.file_attributes() & 0x10 != 0 {
            dir.symlink_dir(target, name)?;
        } else {
            dir.symlink_file(target, name)?;
        }
    }
    Ok(())
}

fn copy_file(source: &Dir, from: &OsStr, target: &Dir, to: &OsStr) -> Result<()> {
    let mut input = regular_file(source, from)?.into_std();
    let mut output = create_file(target, to)?.into_std();
    let m = input.metadata()?;
    io::copy(&mut input, &mut output)?;
    output.set_modified(m.modified()?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(std::fs::Permissions::from_mode(
            m.permissions().mode() & 0o777,
        ))?;
    }
    Ok(())
}

/// Sets the source mode on `target` after copying its contents. The top-level
/// payload passes `apply_mode = false`: renaming a directory to a new parent
/// needs write permission on it, so its mode is applied after publication.
fn copy_dir(
    source: &Dir,
    target: &Dir,
    at: &Path,
    excluded: Option<&Path>,
    apply_mode: bool,
) -> Result<()> {
    for item in source.entries()? {
        let name = item?.file_name();
        let path = at.join(&name);
        if paths::protected_name(&name) || excluded == Some(path.as_path()) {
            continue;
        }
        paths::check_native_destination(&name)?;
        let m = source.symlink_metadata(&name)?;
        if is_link(&m) {
            link(target, &name, &source.read_link_contents(&name)?, &m)
                .with_context(|| format!("recreate link {}", quote(&path)))?;
        } else if m.is_dir() {
            mkdir(target, &name)?;
            copy_dir(
                &source.open_dir_nofollow(&name)?,
                &target.open_dir_nofollow(&name)?,
                &path,
                excluded,
                true,
            )?;
        } else if m.is_file() {
            copy_file(source, &name, target, &name)
                .with_context(|| format!("copy {}", quote(&path)))?;
        } else {
            eprintln!("warning: skipped special file {}", quote(&path));
        }
    }
    #[cfg(unix)]
    if apply_mode {
        set_dir_mode(source, target).context("set staged directory permissions")?;
    }
    #[cfg(not(unix))]
    let _ = apply_mode;
    Ok(())
}

/// Gives `target` the permission bits of `source`.
#[cfg(unix)]
fn set_dir_mode(source: &Dir, target: &Dir) -> Result<()> {
    HeldDir::capture(source, target)?.apply()
}

/// A directory descriptor plus the mode it should get. The descriptor follows
/// the directory across renames, so the mode can be applied after publication
/// without any name lookup that another writer could redirect.
#[cfg(unix)]
struct HeldDir {
    handle: rustix::fd::OwnedFd,
    mode: rustix::fs::Mode,
}

#[cfg(unix)]
impl HeldDir {
    fn capture(source: &Dir, target: &Dir) -> Result<Self> {
        use rustix::fs::{fstat, openat, Mode, OFlags};

        let mode = Mode::from_raw_mode(fstat(source)?.st_mode & 0o777);
        // Capability directories may use O_PATH on Linux, which fchmod
        // rejects. Open a real descriptor relative to the directory.
        let handle = openat(
            target,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self { handle, mode })
    }

    fn apply(self) -> Result<()> {
        rustix::fs::fchmod(self.handle, self.mode)?;
        Ok(())
    }
}

fn apply_entry(
    config: &Config,
    entry: &Entry,
    source: &Dir,
    target: &Dir,
    c: &Checkouts,
    dry: bool,
    names: Names,
) -> Result<()> {
    let name = quote(&entry.path);
    let joined = c.source.join(&entry.source);
    let backup_name = format!(
        "{BACKUP}{}",
        digest(names.recovery_key(&entry.path).as_os_str())
    );
    let existing = parent(target, &entry.path, false)?;
    let mut replacing = false;
    if let Some((dir, leaf)) = &existing {
        if inspect(dir, OsStr::new(&backup_name))?.is_some() {
            let backup = c
                .target
                .join(entry.path.parent().unwrap_or(Path::new("")))
                .join(&backup_name)
                .join("original");
            bail!("displaced original at {}; inspect it and move it back or remove the backup directory before retrying", quote(&backup));
        }
        if let Some(m) = inspect(dir, leaf)? {
            if config.mode == Mode::Symlink
                && is_link(&m)
                && dir.read_link_contents(leaf)? == joined
            {
                println!("already linked {name}");
                return Ok(());
            }
            if !config.overwrite {
                println!("skipped {name} (exists)");
                return Ok(());
            }
            replacing = true;
            if m.is_dir() && !is_link(&m) {
                #[cfg(unix)]
                if let Err(e) = rustix::fs::accessat(
                    dir,
                    *leaf,
                    rustix::fs::Access::WRITE_OK,
                    rustix::fs::AtFlags::empty(),
                ) {
                    if matches!(e, rustix::io::Errno::ACCESS | rustix::io::Errno::ROFS) {
                        bail!("cannot replace {name} (read-only)");
                    }
                    return Err(io::Error::from(e)).context(format!("check {name}"));
                }
                replace::ensure_recoverable(
                    &dir.open_dir_nofollow(leaf)?,
                    &c.target.join(&entry.path),
                )?;
            }
        }
    }
    if dry {
        let verb = if replacing {
            "replace"
        } else if config.mode == Mode::Copy {
            "copy"
        } else {
            "link"
        };
        println!("would {verb} {name}");
        return Ok(());
    }
    let (src, src_name) = source_parent(source, &entry.source)?;
    let (dst, dst_name) = if let Some(pair) = existing {
        pair
    } else {
        parent(target, &entry.path, true)?
            .ok_or_else(|| anyhow!("destination parent does not exist"))?
    };
    let existing = inspect(&dst, dst_name)?;
    if existing.is_some() && !config.overwrite {
        println!("skipped {name} (exists)");
        return Ok(());
    }
    let stage = Stage::new(&dst)?;
    let payload = OsStr::new("payload");
    #[cfg(unix)]
    let mut held = None;
    if config.mode == Mode::Symlink {
        let m = src.symlink_metadata(src_name)?;
        if is_link(&m) || (!m.is_dir() && !m.is_file()) {
            bail!("source changed since preflight");
        }
        link(&stage.dir, payload, &joined, &m)?;
    } else if entry.dir {
        mkdir(&stage.dir, payload)?;
        let from = src.open_dir_nofollow(src_name)?;
        let to = stage.dir.open_dir_nofollow(payload)?;
        #[cfg(unix)]
        {
            held = Some(HeldDir::capture(&from, &to).context("read directory permissions")?);
        }
        copy_dir(
            &from,
            &to,
            &entry.source,
            c.target.strip_prefix(&c.source).ok(),
            false,
        )?;
    } else {
        copy_file(&src, src_name, &stage.dir, payload)?;
    }
    let new_dir = config.mode == Mode::Copy && entry.dir;
    if existing.as_ref().is_some_and(|m| m.is_dir() || new_dir) {
        swap(
            &stage.dir,
            payload,
            &dst,
            dst_name,
            OsStr::new(&backup_name),
        )?;
    } else if !config.overwrite {
        match rename_new(&stage.dir, payload, &dst, dst_name) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                println!("skipped {name} (exists)");
                return Ok(());
            }
            result => result?,
        }
    } else {
        stage.dir.rename(payload, &dst, dst_name)?;
    }
    #[cfg(unix)]
    if let Some(held) = held {
        held.apply()
            .context("installed, but could not set directory permissions")?;
    }
    let verb = if existing.is_some() {
        "replaced"
    } else if config.mode == Mode::Copy {
        "copied"
    } else {
        "linked"
    };
    println!("{verb} {name}");
    Ok(())
}

pub fn files(
    config: &Config,
    entries: &[Entry],
    source: &Dir,
    target: &Dir,
    c: &Checkouts,
    dry: bool,
) -> crate::PhaseStatus {
    let mut status = crate::PhaseStatus::Succeeded;
    let names = Names::for_target(target);
    for entry in entries {
        if let Err(e) = apply_entry(config, entry, source, target, c, dry, names) {
            eprintln!("failed {}: {e:#}", quote(&entry.path));
            status = crate::PhaseStatus::Failed;
        }
    }
    status
}
