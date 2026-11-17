mod apply;
mod checkout;
mod commands;
mod config;
mod fs;
mod lock;
mod paths;
mod plan;
mod replace;

use anyhow::{bail, Context, Result};
use std::{env, ffi::OsStr, path::PathBuf, process::ExitCode};

const HELP: &str =
    "Usage: herdr-wt run [--no-commands] [--dry-run] [PATH]\n       herdr-wt --help | --version";

fn main() -> ExitCode {
    ExitCode::from(match run() {
        Ok(outcome) => outcome as u8,
        Err(err) => {
            eprintln!("error: {err:#}");
            1
        }
    })
}

#[repr(u8)]
enum RunOutcome {
    Success = 0,
    FileFailure = 2,
    CommandFailure = 3,
    SourceRefused = 4,
}

#[derive(PartialEq, Eq)]
enum PhaseStatus {
    Succeeded,
    Failed,
}

fn run() -> Result<RunOutcome> {
    let mut args = env::args_os().skip(1);
    let command = args.next();
    match command.as_deref().and_then(OsStr::to_str) {
        Some("--help" | "-h") => {
            println!("{HELP}");
            return Ok(RunOutcome::Success);
        }
        Some("--version" | "-V") => {
            println!("herdr-wt {}", env!("CARGO_PKG_VERSION"));
            return Ok(RunOutcome::Success);
        }
        Some("run" | "event" | "action") => {}
        _ => bail!("{HELP}"),
    }
    let event = command.as_deref() == Some(OsStr::new("event"));
    let standalone = command.as_deref() == Some(OsStr::new("run"));
    let mut no_commands = false;
    let mut dry = env::var_os("HERDR_WT_DRY_RUN").as_deref() == Some(OsStr::new("1"));
    let mut path = None;
    let mut options = true;
    for arg in args {
        if !standalone {
            bail!("{HELP}");
        }
        match (options, arg.to_str()) {
            (true, Some("--")) => options = false,
            (true, Some("--help" | "-h")) => {
                println!("{HELP}");
                return Ok(RunOutcome::Success);
            }
            (true, Some("--dry-run")) => dry = true,
            (true, Some("--no-commands")) => no_commands = true,
            (true, Some(s)) if s.starts_with('-') => bail!("{HELP}"),
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            _ => bail!("{HELP}"),
        }
    }
    let checkouts = if standalone {
        checkout::standalone(&match path {
            Some(path) => path,
            None => env::current_dir()?,
        })?
    } else {
        checkout::herdr(event)?
    };
    let Some(checkouts) = checkouts else {
        return Ok(RunOutcome::Success);
    };
    if checkout::same(&checkouts.source, &checkouts.target)? {
        eprintln!("refused: target is the source checkout");
        return Ok(RunOutcome::SourceRefused);
    }
    let source = fs::open_root(&checkouts.source).context("open source checkout")?;
    let target = fs::open_root(&checkouts.target).context("open target worktree")?;
    let Some(config) = config::load(&source, &checkouts.source)? else {
        if !event {
            println!("no .herdr-wt.toml in source checkout");
        }
        return Ok(RunOutcome::Success);
    };
    let entries = plan::select(&config, &source, &target, &checkouts)?;
    let _lock = if !dry && (!entries.is_empty() || (!no_commands && !config.commands.is_empty())) {
        Some(lock::lock(&checkouts.target)?)
    } else {
        None
    };
    let failed = apply::files(&config, &entries, &source, &target, &checkouts, dry);
    if commands::run(&config, &checkouts, dry, no_commands) == PhaseStatus::Failed {
        return Ok(RunOutcome::CommandFailure);
    }
    Ok(if failed == PhaseStatus::Failed {
        RunOutcome::FileFailure
    } else {
        RunOutcome::Success
    })
}

fn quote(path: &std::path::Path) -> String {
    let mut out = String::new();
    for c in path.as_os_str().to_string_lossy().chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}
