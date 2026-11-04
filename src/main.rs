use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use fatresize::Error;
use fatresize::cli::{self, Config};

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("fatresize: {error}");
            eprintln!("Try 'fatresize --help' for usage.");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool, Error> {
    let config = cli::parse_from(std::env::args_os().skip(1))?;
    if config.help {
        print!("{}", cli::HELP);
        return Ok(true);
    }
    if config.version {
        println!("fatresize {}", env!("CARGO_PKG_VERSION"));
        return Ok(true);
    }

    if config.dry_run
        && let Some(size) = config.size
    {
        let path = config.device.as_ref().expect("validated device");
        let plan = fatresize::plan_path(path, config.partition, size.bytes())?;
        if !config.quiet {
            if config.json {
                println!("{}", plan.to_json());
            } else {
                print!("{}", plan.to_text());
            }
        }
        return Ok(true);
    }
    if config.dry_run {
        if !config.quiet {
            println!("Dry run: no writes performed.");
        }
        return Ok(true);
    }

    if config.info || config.check {
        let path = config.device.as_ref().expect("validated device");
        let report = if config.check {
            fatresize::inspect_path(path, config.partition)?
        } else {
            fatresize::summarize_path(path, config.partition)?
        };
        if !config.quiet {
            if config.json {
                println!("{}", report.to_json());
            } else {
                print!("{}", report.to_text());
            }
        }
        return Ok(!config.check || report.is_consistent());
    }
    if let Some(recovery) = &config.restore {
        let path = config.device.as_ref().expect("validated device");
        confirm(&config, "Restore saved FAT metadata")?;
        fatresize::restore_path(path, config.partition, recovery, config.force_yes)?;
        if !config.quiet {
            println!("Restore complete.");
        }
        return Ok(true);
    }
    if let Some(size) = config.size {
        let path = config.device.as_ref().expect("validated device");
        let recovery = recovery_path(path, &config)?;
        let target = if size.is_max() {
            "the maximum safe size".to_owned()
        } else if size.is_min() {
            "the minimum safe size".to_owned()
        } else {
            format!("{} bytes", size.bytes())
        };
        confirm(&config, &format!("Resize filesystem to {target}"))?;
        if config.verbose && !config.quiet {
            eprintln!("Recovery record: {}", recovery.display());
        }
        fatresize::resize_path(path, config.partition, size.bytes(), &recovery, |stage| {
            if config.progress && !config.quiet {
                eprintln!("{stage:?}");
            }
        })?;
        if !config.quiet {
            println!("Resize complete. Recovery record: {}", recovery.display());
        }
        return Ok(true);
    }
    if let Some(recovery) = &config.backup {
        let path = config.device.as_ref().expect("validated device");
        fatresize::backup_path(path, config.partition, recovery)?;
        if !config.quiet {
            println!("Backup complete: {}", recovery.display());
        }
        return Ok(true);
    }
    Err(Error::NotImplemented(requested_operation(&config)))
}

fn recovery_path(path: &Path, config: &Config) -> Result<std::path::PathBuf, Error> {
    if let Some(path) = &config.backup {
        return Ok(path.clone());
    }
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw devices require an explicit --backup FILE",
        )
        .into());
    }
    Ok(fatresize::default_recovery_path(path))
}

fn confirm(config: &Config, action: &str) -> Result<(), Error> {
    if config.force_yes {
        return Ok(());
    }
    if config.quiet {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--quiet requires --force-yes for a write operation",
        )
        .into());
    }
    eprint!("{action}? [y/N] ");
    io::stderr().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::Interrupted, "operation cancelled").into())
    }
}

fn requested_operation(config: &Config) -> &'static str {
    if config.restore.is_some() {
        "metadata restore"
    } else if config.check {
        "filesystem checking"
    } else if config.info {
        "filesystem inspection"
    } else if config.size.is_some() {
        "filesystem resizing"
    } else if config.backup.is_some() {
        "metadata backup"
    } else {
        "filesystem operation"
    }
}
