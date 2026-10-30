use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::error::CliError;
use crate::size::Size;

pub const HELP: &str = "\
fatresize - inspect and resize FAT filesystems

Usage: fatresize [OPTIONS] DEVICE

Compatibility options:
  -s, --size SIZE        Resize to SIZE (bytes, K/M/G/T/P binary unit, or max)
  -i, --info             Display filesystem information
  -f, --force-yes        Assume yes for confirmation prompts
  -n, --partition NUM    Select a partition within DEVICE
  -p, --progress         Display progress
  -q, --quiet            Suppress non-error output
  -v, --verbose          Display additional diagnostic output
  -h, --help             Display this help
  -V, --version          Display version information

Enhanced options:
      --dry-run          Plan changes without writing them
      --check            Check filesystem metadata without resizing
      --backup FILE      Save resize metadata to FILE
      --restore FILE     Restore resize metadata from FILE
      --json             Emit machine-readable output
";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub device: Option<PathBuf>,
    pub size: Option<Size>,
    pub info: bool,
    pub force_yes: bool,
    pub partition: Option<u32>,
    pub progress: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub help: bool,
    pub version: bool,
    pub dry_run: bool,
    pub check: bool,
    pub backup: Option<PathBuf>,
    pub restore: Option<PathBuf>,
    pub json: bool,
}

pub fn parse_from<I, T>(arguments: I) -> Result<Config, CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    let mut config = Config::default();
    let mut index = 0;
    let mut positional_only = false;

    while index < arguments.len() {
        let argument = &arguments[index];
        if !positional_only && argument == OsStr::new("--") {
            positional_only = true;
            index += 1;
            continue;
        }

        let text = argument.to_str();
        if !positional_only && text.is_some_and(|value| value.starts_with("--")) {
            let text = text.expect("checked above");
            let (option, inline_value) = text
                .split_once('=')
                .map_or((text, None), |(name, value)| (name, Some(value)));
            match option {
                "--size" => {
                    reject_duplicate(config.size.is_some(), "--size")?;
                    let value = option_value(&arguments, &mut index, inline_value, "--size")?;
                    let value = utf8_value(value, "--size")?;
                    config.size = Some(value.parse().map_err(|error: crate::SizeParseError| {
                        CliError::InvalidValue {
                            option: "--size",
                            value: value.to_owned(),
                            reason: error.to_string(),
                        }
                    })?);
                }
                "--partition" => {
                    reject_duplicate(config.partition.is_some(), "--partition")?;
                    let value = option_value(&arguments, &mut index, inline_value, "--partition")?;
                    let value = utf8_value(value, "--partition")?;
                    let partition =
                        value
                            .parse::<u32>()
                            .map_err(|error| CliError::InvalidValue {
                                option: "--partition",
                                value: value.to_owned(),
                                reason: error.to_string(),
                            })?;
                    if partition == 0 {
                        return Err(CliError::InvalidValue {
                            option: "--partition",
                            value: value.to_owned(),
                            reason: "partition numbers start at 1".to_owned(),
                        });
                    }
                    config.partition = Some(partition);
                }
                "--backup" => {
                    reject_duplicate(config.backup.is_some(), "--backup")?;
                    config.backup = Some(PathBuf::from(option_value(
                        &arguments,
                        &mut index,
                        inline_value,
                        "--backup",
                    )?));
                }
                "--restore" => {
                    reject_duplicate(config.restore.is_some(), "--restore")?;
                    config.restore = Some(PathBuf::from(option_value(
                        &arguments,
                        &mut index,
                        inline_value,
                        "--restore",
                    )?));
                }
                "--info" => set_switch(&mut config.info, inline_value, "--info")?,
                "--force-yes" => set_switch(&mut config.force_yes, inline_value, "--force-yes")?,
                "--progress" => set_switch(&mut config.progress, inline_value, "--progress")?,
                "--quiet" => set_switch(&mut config.quiet, inline_value, "--quiet")?,
                "--verbose" => set_switch(&mut config.verbose, inline_value, "--verbose")?,
                "--help" => set_switch(&mut config.help, inline_value, "--help")?,
                "--version" => set_switch(&mut config.version, inline_value, "--version")?,
                "--dry-run" => set_switch(&mut config.dry_run, inline_value, "--dry-run")?,
                "--check" => set_switch(&mut config.check, inline_value, "--check")?,
                "--json" => set_switch(&mut config.json, inline_value, "--json")?,
                _ => return Err(CliError::UnknownOption(option.to_owned())),
            }
        } else if !positional_only && text.is_some_and(is_short_option) {
            parse_short_option(&arguments, &mut index, &mut config)?;
        } else {
            if text.is_none() && argument.to_string_lossy().starts_with('-') {
                return Err(CliError::NonUnicodeOption);
            }
            if config.device.is_some() {
                return Err(CliError::UnexpectedArgument(
                    argument.to_string_lossy().into_owned(),
                ));
            }
            config.device = Some(PathBuf::from(argument.as_os_str()));
        }
        index += 1;
    }

    validate(config)
}

fn is_short_option(value: &str) -> bool {
    value.starts_with('-') && value != "-"
}

fn parse_short_option(
    arguments: &[OsString],
    index: &mut usize,
    config: &mut Config,
) -> Result<(), CliError> {
    let text = arguments[*index]
        .to_str()
        .ok_or(CliError::NonUnicodeOption)?;
    let options = text[1..].char_indices();

    for (offset, option) in options {
        match option {
            'i' => reject_and_set(&mut config.info, "--info")?,
            'f' => reject_and_set(&mut config.force_yes, "--force-yes")?,
            'p' => reject_and_set(&mut config.progress, "--progress")?,
            'q' => reject_and_set(&mut config.quiet, "--quiet")?,
            'v' => reject_and_set(&mut config.verbose, "--verbose")?,
            'h' => reject_and_set(&mut config.help, "--help")?,
            'V' => reject_and_set(&mut config.version, "--version")?,
            's' | 'n' => {
                let name = if option == 's' {
                    "--size"
                } else {
                    "--partition"
                };
                let value_start = 1 + offset + option.len_utf8();
                let value = if value_start < text.len() {
                    OsString::from(&text[value_start..])
                } else {
                    *index += 1;
                    arguments
                        .get(*index)
                        .cloned()
                        .ok_or(CliError::MissingValue(name))?
                };
                let value = utf8_value(value, name)?;
                if option == 's' {
                    reject_duplicate(config.size.is_some(), "--size")?;
                    config.size = Some(value.parse().map_err(|error: crate::SizeParseError| {
                        CliError::InvalidValue {
                            option: "--size",
                            value: value.to_owned(),
                            reason: error.to_string(),
                        }
                    })?);
                } else {
                    reject_duplicate(config.partition.is_some(), "--partition")?;
                    let partition =
                        value
                            .parse::<u32>()
                            .map_err(|error| CliError::InvalidValue {
                                option: "--partition",
                                value: value.to_owned(),
                                reason: error.to_string(),
                            })?;
                    if partition == 0 {
                        return Err(CliError::InvalidValue {
                            option: "--partition",
                            value: value.to_owned(),
                            reason: "partition numbers start at 1".to_owned(),
                        });
                    }
                    config.partition = Some(partition);
                }
                return Ok(());
            }
            _ => return Err(CliError::UnknownOption(format!("-{option}"))),
        }
    }
    Ok(())
}

fn option_value(
    arguments: &[OsString],
    index: &mut usize,
    inline: Option<&str>,
    option: &'static str,
) -> Result<OsString, CliError> {
    if let Some(value) = inline {
        if value.is_empty() {
            return Err(CliError::MissingValue(option));
        }
        return Ok(OsString::from(value));
    }
    *index += 1;
    arguments
        .get(*index)
        .cloned()
        .ok_or(CliError::MissingValue(option))
}

fn utf8_value(value: OsString, option: &'static str) -> Result<String, CliError> {
    value.into_string().map_err(|value| CliError::InvalidValue {
        option,
        value: value.to_string_lossy().into_owned(),
        reason: "value must be valid UTF-8".to_owned(),
    })
}

fn set_switch(
    destination: &mut bool,
    inline: Option<&str>,
    option: &'static str,
) -> Result<(), CliError> {
    if let Some(value) = inline {
        return Err(CliError::InvalidValue {
            option,
            value: value.to_owned(),
            reason: "this option does not take a value".to_owned(),
        });
    }
    reject_and_set(destination, option)
}

fn reject_and_set(destination: &mut bool, option: &'static str) -> Result<(), CliError> {
    reject_duplicate(*destination, option)?;
    *destination = true;
    Ok(())
}

fn reject_duplicate(is_set: bool, option: &'static str) -> Result<(), CliError> {
    if is_set {
        Err(CliError::DuplicateOption(option))
    } else {
        Ok(())
    }
}

fn validate(config: Config) -> Result<Config, CliError> {
    if config.help || config.version {
        return Ok(config);
    }
    if config.device.is_none() {
        return Err(CliError::MissingDevice);
    }
    if config.quiet && config.verbose {
        return Err(CliError::Conflict("--quiet", "--verbose"));
    }
    if config.quiet && config.json {
        return Err(CliError::Conflict("--quiet", "--json"));
    }
    if config.backup.is_some() && config.restore.is_some() {
        return Err(CliError::Conflict("--backup", "--restore"));
    }
    if config.size.is_some() && config.restore.is_some() {
        return Err(CliError::Conflict("--size", "--restore"));
    }
    if config.size.is_some() && config.info {
        return Err(CliError::Conflict("--size", "--info"));
    }
    if config.size.is_some() && config.check {
        return Err(CliError::Conflict("--size", "--check"));
    }
    if config.restore.is_some() && config.info {
        return Err(CliError::Conflict("--restore", "--info"));
    }
    if config.restore.is_some() && config.check {
        return Err(CliError::Conflict("--restore", "--check"));
    }
    if config.backup.is_some() && config.size.is_none() && (config.info || config.check) {
        return Err(CliError::Conflict(
            "--backup",
            if config.info { "--info" } else { "--check" },
        ));
    }
    if config.dry_run && config.size.is_none() {
        return Err(CliError::Conflict("--dry-run", "operation without --size"));
    }
    if config.dry_run && config.backup.is_some() {
        return Err(CliError::Conflict("--dry-run", "--backup"));
    }
    if config.progress && config.size.is_none() {
        return Err(CliError::Conflict("--progress", "operation without --size"));
    }
    if config.json && !(config.info || config.check || (config.dry_run && config.size.is_some())) {
        return Err(CliError::Conflict(
            "--json",
            "operation without structured output",
        ));
    }
    if config.size.is_none()
        && !config.info
        && !config.check
        && config.backup.is_none()
        && config.restore.is_none()
    {
        return Err(CliError::MissingOperation);
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_and_enhanced_options() {
        let config = parse_from([
            "--size=1.5GiB",
            "--force-yes",
            "--partition",
            "2",
            "--progress",
            "--verbose",
            "--backup",
            "metadata.bak",
            "/dev/example",
        ])
        .unwrap();

        assert_eq!(config.size.unwrap().bytes(), 1_610_612_736);
        assert_eq!(config.partition, Some(2));
        assert_eq!(config.device, Some(PathBuf::from("/dev/example")));
        assert!(config.force_yes && config.progress && config.verbose);
        assert_eq!(config.backup, Some(PathBuf::from("metadata.bak")));
    }

    #[test]
    fn parses_short_options_and_separator() {
        let config = parse_from(["-fpv", "-s2M", "-n", "3", "--", "-device"]).unwrap();
        assert!(config.force_yes && config.progress && config.verbose);
        assert_eq!(config.size.unwrap().bytes(), 2 * 1024 * 1024);
        assert_eq!(config.partition, Some(3));
        assert_eq!(config.device, Some(PathBuf::from("-device")));
    }

    #[test]
    fn help_and_version_do_not_require_a_device() {
        assert!(parse_from(["--help"]).unwrap().help);
        assert!(parse_from(["-V"]).unwrap().version);
        assert!(parse_from(["--version"]).unwrap().version);
    }

    #[test]
    fn rejects_missing_values_duplicates_and_conflicts() {
        assert_eq!(
            parse_from(["--size"]),
            Err(CliError::MissingValue("--size"))
        );
        assert_eq!(
            parse_from(["--info", "--info", "device"]),
            Err(CliError::DuplicateOption("--info"))
        );
        assert_eq!(
            parse_from(["--quiet", "--verbose", "device"]),
            Err(CliError::Conflict("--quiet", "--verbose"))
        );
        assert_eq!(
            parse_from(["--backup=a", "--restore=b", "device"]),
            Err(CliError::Conflict("--backup", "--restore"))
        );
    }

    #[test]
    fn rejects_unknown_options_and_extra_devices() {
        assert_eq!(
            parse_from(["--wat", "device"]),
            Err(CliError::UnknownOption("--wat".to_owned()))
        );
        assert_eq!(
            parse_from(["one", "two"]),
            Err(CliError::UnexpectedArgument("two".to_owned()))
        );
    }

    #[test]
    fn validates_operation_modes() {
        assert_eq!(
            parse_from(["--dry-run", "device"]),
            Err(CliError::Conflict("--dry-run", "operation without --size"))
        );
        assert_eq!(
            parse_from(["--size", "1M", "--check", "device"]),
            Err(CliError::Conflict("--size", "--check"))
        );
        assert_eq!(
            parse_from(["--info", "--quiet", "--json", "device"]),
            Err(CliError::Conflict("--quiet", "--json"))
        );
        assert_eq!(parse_from(["device"]), Err(CliError::MissingOperation));
        assert!(parse_from(["--size", "max", "--dry-run", "device"]).is_ok());
        assert!(parse_from(["--info", "--check", "--json", "device"]).is_ok());
    }
}
