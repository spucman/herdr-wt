use crate::{fs, paths::resolve};
use anyhow::{bail, Context, Result};
use cap_std::fs::Dir;
use serde::Deserialize;
use std::{
    io::{self, Read},
    path::Path,
};
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
    pub files: Vec<String>,
    pub commands: Vec<String>,
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
    let (parent, leaf) = fs::source_parent(source, &path)?;
    let mut contents = String::new();
    fs::regular_file(&parent, leaf)
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
