mod apply;
mod checkout;
mod commands;
mod config;
mod fs;
mod lock;
mod paths;
mod plan;
mod replace;

use anyhow::{Context, Result};
use std::{
    env,
    ffi::OsStr,
    io::Write,
    panic,
    path::{Path, PathBuf},
    process::ExitCode,
};

const HELP: &str =
    "Usage: herdr-wt run [--no-commands] [--dry-run] [PATH]\n       herdr-wt --help | --version";

fn main() -> ExitCode {
    panic::set_hook(Box::new(|info| {
        let _ = writeln!(
            std::io::stderr(),
            "error: {}",
            panic_text(info.payload(), info.location())
        );
    }));
    ExitCode::from(match panic::catch_unwind(run) {
        Ok(Ok(outcome)) => outcome as u8,
        Ok(Err(err)) => {
            let _ = writeln!(std::io::stderr(), "error: {err:#}");
            1
        }
        Err(_) => 1,
    })
}

fn usage(message: String) -> anyhow::Error {
    anyhow::anyhow!("{message}\n{HELP}")
}

fn unknown_command(command: Option<&OsStr>) -> anyhow::Error {
    let Some(command) = command else {
        return usage("missing subcommand".into());
    };
    let quoted = quote(Path::new(command));
    if command.as_encoded_bytes().starts_with(b"-") {
        usage(format!("unknown option {quoted}"))
    } else {
        usage(format!("unknown command {quoted}"))
    }
}

fn panic_text(
    payload: &(dyn std::any::Any + Send),
    location: Option<&panic::Location<'_>>,
) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic");
    let location = location.map_or_else(
        || "unknown location".to_owned(),
        |location| format!("{}:{}", location.file(), location.line()),
    );
    format!(
        "internal error: {} at {location}",
        quote(Path::new(message))
    )
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
        _ => return Err(unknown_command(command.as_deref())),
    }
    let event = command.as_deref() == Some(OsStr::new("event"));
    let standalone = command.as_deref() == Some(OsStr::new("run"));
    let mut no_commands = false;
    let mut dry = env::var_os("HERDR_WT_DRY_RUN").as_deref() == Some(OsStr::new("1"));
    let mut path = None;
    let mut options = true;
    for arg in args {
        if !standalone {
            let name = if event { "event" } else { "action" };
            return Err(usage(format!("{name} takes no arguments")));
        }
        match (options, arg.to_str()) {
            (true, Some("--")) => options = false,
            (true, Some("--help" | "-h")) => {
                println!("{HELP}");
                return Ok(RunOutcome::Success);
            }
            (true, Some("--dry-run")) => dry = true,
            (true, Some("--no-commands")) => no_commands = true,
            (true, Some(s)) if s.starts_with('-') => {
                return Err(usage(format!("unknown option {}", quote(Path::new(s)))));
            }
            _ if path.is_none() => path = Some(PathBuf::from(arg)),
            _ => {
                return Err(usage(format!(
                    "unexpected argument {}",
                    quote(Path::new(&arg))
                )));
            }
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

fn quote(path: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_escapes_control_characters() {
        assert_eq!(
            quote(Path::new("a\nb\u{1b}[0m c\u{e9}")),
            "a\\nb\\u{1b}[0m c\u{e9}"
        );
    }

    #[test]
    fn panic_text_names_str_and_string_payloads() {
        let location = Some(std::panic::Location::caller());

        let from_str = panic_text(&"boom", location);
        let from_string = panic_text(&String::from("boom"), location);

        for text in [from_str, from_string] {
            assert!(text.contains("boom at "), "{text}");
            assert!(text.contains(file!()), "{text}");
        }
    }

    #[test]
    fn panic_text_escapes_control_characters() {
        let text = panic_text(&"a\nb", None);

        assert!(text.contains("a\\nb"), "{text}");
        assert!(!text.contains('\n'), "{text}");
    }

    #[test]
    fn panic_text_falls_back_for_other_payloads() {
        assert_eq!(
            panic_text(&42_i32, None),
            "internal error: unknown panic at unknown location"
        );
    }
}
