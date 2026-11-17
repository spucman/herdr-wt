use crate::{
    checkout::{self, Checkouts},
    config::Config,
    quote,
};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Instant,
};
pub fn run(config: &Config, c: &Checkouts, dry: bool, skip: bool) -> crate::PhaseStatus {
    if config.commands.is_empty() {
        return crate::PhaseStatus::Succeeded;
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
    if failed {
        crate::PhaseStatus::Failed
    } else {
        crate::PhaseStatus::Succeeded
    }
}
