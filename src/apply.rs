use crate::{
    checkout::{self, Checkouts},
    config::{self, Config, Entry, Mode},
    quote,
};
use anyhow::{anyhow, bail, Context, Result};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, DirBuilder, File, Metadata, OpenOptions},
};
use fs4::fs_std::FileExt;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Instant,
};

const STAGING: &str = ".herdr-wt-tmp-stage-";
const BACKUP: &str = ".herdr-wt-tmp-original-";

pub fn open_root(path: &Path) -> io::Result<Dir> {
    Dir::open_ambient_dir(path, ambient_authority())
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

fn inspect(dir: &Dir, name: &OsStr) -> io::Result<Option<Metadata>> {
    match dir.symlink_metadata(name) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn mkdir(dir: &Dir, name: &OsStr) -> io::Result<()> {
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

fn parent<'a>(root: &Dir, path: &'a Path, create: bool) -> Result<Option<(Dir, &'a OsStr)>> {
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
        options.custom_flags(libc::O_NONBLOCK);
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

fn create_file(dir: &Dir, name: &OsStr) -> io::Result<File> {
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

fn digest(value: &OsStr) -> String {
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
    #[cfg(unix)]
    let identity = {
        use cap_std::fs::MetadataExt;
        let m = root.dir_metadata()?;
        format!("{}:{}", m.dev(), m.ino())
    };
    #[cfg(windows)]
    let identity = {
        let m = winapi_util::file::information(root.into_std_file())?;
        format!("{}:{}", m.volume_serial_number(), m.file_index())
    };
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
        options.mode(0o600).custom_flags(libc::O_NONBLOCK);
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
    directories::BaseDirs::new().map(|dirs| dirs.data_local_dir().to_owned())
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

struct Stage<'a> {
    parent: &'a Dir,
    name: String,
    dir: Dir,
}

impl<'a> Stage<'a> {
    fn new(parent: &'a Dir) -> Result<Self> {
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
        output.set_permissions(fs::Permissions::from_mode(m.permissions().mode() & 0o777))?;
    }
    Ok(())
}

fn copy_dir(source: &Dir, target: &Dir, at: &Path, excluded: Option<&Path>) -> Result<()> {
    for item in source.entries()? {
        let name = item?.file_name();
        let path = at.join(&name);
        if config::protected_name(&name) || excluded == Some(path.as_path()) {
            continue;
        }
        config::check_native_destination(&name)?;
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
            )?;
        } else if m.is_file() {
            copy_file(source, &name, target, &name)
                .with_context(|| format!("copy {}", quote(&path)))?;
        } else {
            eprintln!("warning: skipped special file {}", quote(&path));
        }
    }
    #[cfg(unix)]
    {
        use rustix::fs::{fchmod, fstat, openat, Mode, OFlags};

        let mode = Mode::from_raw_mode(fstat(source)?.st_mode & 0o777);
        // Capability directories may use O_PATH on Linux, which fchmod
        // rejects. Open a real descriptor relative to the staged directory.
        let handle = openat(
            target,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        fchmod(handle, mode).context("set staged directory permissions")?;
    }
    Ok(())
}

fn apply_entry(
    config: &Config,
    entry: &Entry,
    source: &Dir,
    target: &Dir,
    c: &Checkouts,
    dry: bool,
) -> Result<()> {
    let name = quote(&entry.path);
    let joined = c.source.join(&entry.source);
    let backup_name = format!(
        "{BACKUP}{}",
        digest(target_key(target, &entry.path).as_os_str())
    );
    let existing = parent(target, &entry.path, false)?;
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
        }
    }
    if dry {
        println!(
            "would {} {name}",
            if config.mode == Mode::Copy {
                "copy"
            } else {
                "link"
            }
        );
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
    if config.mode == Mode::Symlink {
        let m = src.symlink_metadata(src_name)?;
        if is_link(&m) || (!m.is_dir() && !m.is_file()) {
            bail!("source changed since preflight");
        }
        link(&stage.dir, payload, &joined, &m)?;
    } else if entry.dir {
        mkdir(&stage.dir, payload)?;
        copy_dir(
            &src.open_dir_nofollow(src_name)?,
            &stage.dir.open_dir_nofollow(payload)?,
            &entry.source,
            c.target.strip_prefix(&c.source).ok(),
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
    println!(
        "{} {name}",
        if config.mode == Mode::Copy {
            "copied"
        } else {
            "linked"
        }
    );
    Ok(())
}

fn target_key(target: &Dir, path: &Path) -> PathBuf {
    config::Names::for_target(target)
        .key(path)
        .into_iter()
        .collect()
}

fn swap(from: &Dir, payload: &OsStr, dst: &Dir, name: &OsStr, backup: &OsStr) -> Result<()> {
    mkdir(dst, backup)?;
    let original = OsStr::new("original");
    let backup_dir = dst.open_dir_nofollow(backup)?;
    if let Err(e) = dst.rename(name, &backup_dir, original) {
        let _ = dst.remove_dir(backup);
        return Err(e).context("move old destination aside");
    }
    if let Err(e) = rename_new(from, payload, dst, name) {
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
fn rename_new(from: &Dir, payload: &OsStr, dst: &Dir, name: &OsStr) -> io::Result<()> {
    rustix::fs::renameat_with(from, payload, dst, name, rustix::fs::RenameFlags::NOREPLACE)
        .map_err(Into::into)
}

#[cfg(windows)]
fn rename_new(from: &Dir, payload: &OsStr, dst: &Dir, name: &OsStr) -> io::Result<()> {
    if inspect(dst, name)?.is_some() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    from.rename(payload, dst, name)
}

pub fn files(
    config: &Config,
    entries: &[Entry],
    source: &Dir,
    target: &Dir,
    c: &Checkouts,
    dry: bool,
) -> bool {
    let mut failed = false;
    for entry in entries {
        if let Err(e) = apply_entry(config, entry, source, target, c, dry) {
            eprintln!("failed {}: {e:#}", quote(&entry.path));
            failed = true;
        }
    }
    failed
}

pub fn commands(config: &Config, c: &Checkouts, dry: bool, skip: bool) -> bool {
    if config.commands.is_empty() {
        return false;
    }
    let git_vars = if !dry && !skip {
        checkout::git(&c.target, &["rev-parse", "--local-env-vars"])
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut failed = false;
    let total = config.commands.len();
    for (i, script) in config.commands.iter().enumerate() {
        let n = i + 1;
        if skip || failed {
            println!("skipped command {n}/{total}");
            continue;
        }
        if dry {
            println!("would run {n}/{total} {}", quote(Path::new(script)));
            continue;
        }
        println!("run {n}/{total}");
        let start = Instant::now();
        #[cfg(unix)]
        let mut process = {
            let mut p = Command::new("sh");
            p.args(["-c", script]).env("PWD", &c.target);
            p
        };
        #[cfg(windows)]
        let mut process = {
            use std::os::windows::process::CommandExt;
            let mut p = Command::new(
                std::env::var_os("COMSPEC")
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "cmd.exe".into()),
            );
            p.args(["/d", "/s", "/c"]).raw_arg(format!("\"{script}\""));
            p
        };
        checkout::clean_git_env(&mut process);
        for var in git_vars
            .lines()
            .filter(|s| s.starts_with("GIT_") && !s.contains('='))
        {
            process.env_remove(var);
        }
        let status = process
            .current_dir(&c.target)
            .stdin(Stdio::null())
            .env("HERDR_WT_SOURCE", &c.source)
            .env("HERDR_WT_TARGET", &c.target)
            .status();
        match status {
            Ok(status) if status.success() => {
                println!("ok {n}/{total} ({:.3}s)", start.elapsed().as_secs_f64())
            }
            Ok(status) => {
                failed = true;
                if let Some(code) = status.code() {
                    eprintln!("command failed {n}/{total} exit {code}");
                } else {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        eprintln!(
                            "command failed {n}/{total} signal {}",
                            status.signal().unwrap_or(0)
                        );
                    }
                    #[cfg(windows)]
                    eprintln!("command failed {n}/{total}: {status}");
                }
            }
            Err(e) => {
                failed = true;
                eprintln!("command failed {n}/{total}: {e}");
            }
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::*;

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
