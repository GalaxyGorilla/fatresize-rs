use std::ffi::OsString;
use std::path::PathBuf;

use fatresize::Size;
use fatresize::cli::{self, Config};
use fatresize::error::SizeParseError;
use fatresize::{CliError, Error};

fn parse(arguments: &[&str]) -> Result<Config, CliError> {
    cli::parse_from(arguments.iter().copied())
}

fn size(value: &str) -> Result<u64, SizeParseError> {
    value.parse::<Size>().map(Size::bytes)
}

#[test]
fn parses_every_documented_size_unit_and_fraction() {
    for (text, expected) in [
        ("0", 0),
        ("1B", 1),
        ("1b", 1),
        ("1K", 1024),
        ("1k", 1024),
        ("1KB", 1024),
        ("1kb", 1024),
        ("1KiB", 1024),
        ("1Ki", 1024),
        ("1M", 1 << 20),
        ("1MiB", 1 << 20),
        ("1G", 1 << 30),
        ("1GiB", 1 << 30),
        ("1T", 1u64 << 40),
        ("1TiB", 1u64 << 40),
        ("1P", 1u64 << 50),
        ("1PiB", 1u64 << 50),
        ("1.5KiB", 1536),
        ("0.5K", 512),
        ("2.000K", 2048),
        ("0.25MiB", 256 * 1024),
        ("max", u64::MAX),
        ("MAX", u64::MAX),
        ("Max", u64::MAX),
        ("min", u64::MAX - 1),
        ("MIN", u64::MAX - 1),
        ("Min", u64::MAX - 1),
    ] {
        assert_eq!(size(text), Ok(expected), "size {text:?}");
    }
}

#[test]
fn rejects_malformed_and_overflowing_sizes() {
    for (text, expected) in [
        ("", SizeParseError::Empty),
        (" ", SizeParseError::InvalidNumber),
        (" 1K", SizeParseError::InvalidNumber),
        ("1K ", SizeParseError::InvalidNumber),
        ("+1K", SizeParseError::InvalidNumber),
        ("-1K", SizeParseError::InvalidNumber),
        (".5K", SizeParseError::InvalidNumber),
        ("1.K", SizeParseError::InvalidNumber),
        ("1..5K", SizeParseError::InvalidNumber),
        ("1.5.5K", SizeParseError::InvalidNumber),
        ("1.1B", SizeParseError::FractionalByte),
        ("0.001K", SizeParseError::FractionalByte),
        ("1.0001KiB", SizeParseError::FractionalByte),
        ("1e3", SizeParseError::UnknownSuffix("e3".to_owned())),
        ("12XB", SizeParseError::UnknownSuffix("XB".to_owned())),
        ("1Kibx", SizeParseError::UnknownSuffix("Kibx".to_owned())),
        ("18446744073709551616", SizeParseError::Overflow),
        ("18446744073709551615K", SizeParseError::Overflow),
        (
            "0.00000000000000000000000000000001K",
            SizeParseError::Overflow,
        ),
    ] {
        assert_eq!(size(text), Err(expected), "size {text:?}");
    }
}

#[test]
fn size_display_round_trips_max_and_bytes() {
    assert_eq!(Size::from_bytes(0).to_string(), "0");
    assert_eq!(Size::from_bytes(1234).to_string(), "1234");
    assert_eq!(Size::from_bytes(Size::MAX).to_string(), "max");
    assert_eq!(Size::from_bytes(Size::MIN).to_string(), "min");
    assert!(Size::from_bytes(Size::MAX).is_max());
    assert!(Size::from_bytes(Size::MIN).is_min());
    assert!(!Size::from_bytes(1).is_max());
    assert!(!Size::from_bytes(1).is_min());
}

#[test]
fn parses_all_long_options_inline_and_separate() {
    let config = parse(&[
        "--size=1.5GiB",
        "--force-yes",
        "--partition=2",
        "--progress",
        "--verbose",
        "--backup=/tmp/meta",
        "/dev/example",
    ])
    .unwrap();
    assert_eq!(config.size, Some(Size::from_bytes(1_610_612_736)));
    assert_eq!(config.partition, Some(2));
    assert_eq!(config.device, Some(PathBuf::from("/dev/example")));
    assert_eq!(config.backup, Some(PathBuf::from("/tmp/meta")));
    assert!(config.force_yes && config.progress && config.verbose && !config.dry_run);

    let config = parse(&["--dry-run", "--size", "1M", "device"]).unwrap();
    assert!(config.dry_run);

    let config = parse(&[
        "--size",
        "2MiB",
        "--partition",
        "3",
        "--backup",
        "meta",
        "--restore",
        "meta",
        "device",
    ]);
    assert_eq!(config, Err(CliError::Conflict("--backup", "--restore")));
}

#[test]
fn parses_grouped_short_options_and_attached_values() {
    let config = parse(&["-ifn2", "device"]).unwrap();
    assert!(config.info && config.force_yes);
    assert_eq!(config.partition, Some(2));
    assert_eq!(config.size, None);

    let config = parse(&["-fpv", "-s2M", "device"]).unwrap();
    assert!(config.force_yes && config.progress && config.verbose);
    assert_eq!(config.size, Some(Size::from_bytes(2 * 1024 * 1024)));

    let config = parse(&["-s", "3K", "-n", "4", "-f", "device"]).unwrap();
    assert_eq!(config.size, Some(Size::from_bytes(3 * 1024)));
    assert_eq!(config.partition, Some(4));
    assert!(config.force_yes);

    let config = parse(&["-h"]).unwrap();
    assert!(config.help);
    let config = parse(&["-V"]).unwrap();
    assert!(config.version);
}

#[test]
fn help_and_version_need_no_device_or_operation() {
    assert!(parse(&["--help"]).unwrap().help);
    assert!(parse(&["--version"]).unwrap().version);
    assert!(parse(&["-h", "--verbose"]).unwrap().help);
    assert!(parse(&["--version", "--json"]).unwrap().version);
}

#[test]
fn end_of_options_and_single_dash_are_devices() {
    let config = parse(&["--", "-device"]).unwrap_err();
    assert_eq!(config, CliError::MissingOperation);
    let config = parse(&["--info", "--", "-device"]).unwrap();
    assert_eq!(config.device, Some(PathBuf::from("-device")));
    assert!(config.info);
    let config = parse(&["-"]).unwrap_err();
    assert_eq!(config, CliError::MissingOperation);
    let config = parse(&["--info", "-"]).unwrap();
    assert_eq!(config.device, Some(PathBuf::from("-")));
    let config = parse(&["--", "--info"]).unwrap_err();
    assert_eq!(config, CliError::MissingOperation);
    let config = parse(&["--", "-device", "--info"]).unwrap_err();
    assert_eq!(config, CliError::UnexpectedArgument("--info".to_owned()));
}

#[test]
fn rejects_bad_values_missing_values_and_duplicates() {
    assert_eq!(parse(&["--size"]), Err(CliError::MissingValue("--size")));
    assert_eq!(parse(&["--size="]), Err(CliError::MissingValue("--size")));
    assert_eq!(
        parse(&["--partition="]),
        Err(CliError::MissingValue("--partition"))
    );
    assert_eq!(
        parse(&["--backup="]),
        Err(CliError::MissingValue("--backup"))
    );
    assert_eq!(
        parse(&["--restore"]),
        Err(CliError::MissingValue("--restore"))
    );
    assert_eq!(
        parse(&["--partition", "0", "--info", "device"]),
        Err(CliError::InvalidValue {
            option: "--partition",
            value: "0".to_owned(),
            reason: "partition numbers start at 1".to_owned(),
        })
    );
    for value in ["x", "-1", "4294967296"] {
        assert!(matches!(
            parse(&["--partition", value, "--info", "device"]),
            Err(CliError::InvalidValue {
                option: "--partition",
                ..
            })
        ));
    }
    assert!(matches!(
        parse(&["--size", "nope", "--dry-run", "device"]),
        Err(CliError::InvalidValue {
            option: "--size",
            ..
        })
    ));
    assert_eq!(
        parse(&["--info=1", "device"]),
        Err(CliError::InvalidValue {
            option: "--info",
            value: "1".to_owned(),
            reason: "this option does not take a value".to_owned(),
        })
    );
    for name in [
        "--size",
        "--partition",
        "--backup",
        "--restore",
        "--info",
        "--check",
        "--force-yes",
        "--progress",
        "--quiet",
        "--verbose",
        "--dry-run",
        "--json",
    ] {
        let arguments: Vec<OsString> = vec![
            OsString::from(name),
            OsString::from("1"),
            OsString::from(name),
            OsString::from("1"),
            OsString::from("device"),
        ];
        assert!(
            matches!(
                cli::parse_from(arguments),
                Err(CliError::DuplicateOption(_))
            ),
            "duplicate {name}"
        );
    }
}

#[test]
fn rejects_unknown_options_and_extra_positionals() {
    assert_eq!(
        parse(&["--wat", "device"]),
        Err(CliError::UnknownOption("--wat".to_owned()))
    );
    assert_eq!(
        parse(&["--wat=1", "device"]),
        Err(CliError::UnknownOption("--wat".to_owned()))
    );
    assert_eq!(
        parse(&["--=1", "device"]),
        Err(CliError::UnknownOption("--".to_owned()))
    );
    assert_eq!(
        parse(&["-x", "device"]),
        Err(CliError::UnknownOption("-x".to_owned()))
    );
    assert_eq!(
        parse(&["one", "two"]),
        Err(CliError::UnexpectedArgument("two".to_owned()))
    );
    assert_eq!(
        parse(&["--info", "one", "--", "two"]),
        Err(CliError::UnexpectedArgument("two".to_owned()))
    );
}

#[test]
fn validates_operation_combinations() {
    assert_eq!(parse(&[]), Err(CliError::MissingDevice));
    assert_eq!(parse(&["device"]), Err(CliError::MissingOperation));
    assert_eq!(
        parse(&["--dry-run", "device"]),
        Err(CliError::Conflict("--dry-run", "operation without --size"))
    );
    assert_eq!(
        parse(&["--dry-run", "--backup", "m", "--size", "1M", "device"]),
        Err(CliError::Conflict("--dry-run", "--backup"))
    );
    assert_eq!(
        parse(&["--progress", "device"]),
        Err(CliError::Conflict("--progress", "operation without --size"))
    );
    assert_eq!(
        parse(&["--json", "--restore", "m", "device"]),
        Err(CliError::Conflict(
            "--json",
            "operation without structured output"
        ))
    );
    for (left, right) in [
        ("--size", "--info"),
        ("--size", "--check"),
        ("--size", "--restore"),
        ("--restore", "--info"),
        ("--restore", "--check"),
        ("--backup", "--restore"),
    ] {
        let mut arguments = vec![left];
        if left == "--size" {
            arguments.push("1M");
        } else if left == "--backup" || left == "--restore" {
            arguments.push("file");
        }
        arguments.push(right);
        if right == "--size" {
            arguments.push("1M");
        } else if right == "--backup" || right == "--restore" {
            arguments.push("file");
        }
        arguments.push("device");
        assert!(
            matches!(cli::parse_from(arguments), Err(CliError::Conflict(..))),
            "{left} with {right}"
        );
    }
    assert_eq!(
        parse(&["--backup", "m", "--info", "device"]),
        Err(CliError::Conflict("--backup", "--info"))
    );
    assert_eq!(
        parse(&["--backup", "m", "--check", "device"]),
        Err(CliError::Conflict("--backup", "--check"))
    );
    assert_eq!(
        parse(&["--quiet", "--verbose", "device"]),
        Err(CliError::Conflict("--quiet", "--verbose"))
    );
    assert_eq!(
        parse(&["--quiet", "--json", "--info", "device"]),
        Err(CliError::Conflict("--quiet", "--json"))
    );

    for arguments in [
        vec!["--info", "device"],
        vec!["--check", "device"],
        vec!["--info", "--check", "device"],
        vec!["--info", "--json", "device"],
        vec!["--check", "--json", "device"],
        vec!["--size", "1M", "--dry-run", "device"],
        vec!["--size", "1M", "--dry-run", "--json", "device"],
        vec!["--size", "1M", "--progress", "--force-yes", "device"],
        vec!["--backup", "file", "device"],
        vec!["--restore", "file", "device"],
        vec!["--size", "max", "--force-yes", "device"],
    ] {
        assert!(
            cli::parse_from(&arguments).is_ok(),
            "expected success for {arguments:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn accepts_non_utf8_device_but_rejects_non_utf8_options_and_values() {
    use std::os::unix::ffi::OsStringExt;

    let device = OsString::from_vec(vec![b'/', 0xff, b'x']);
    let config = cli::parse_from([OsString::from("--info"), device.clone()]).unwrap();
    assert_eq!(config.device, Some(PathBuf::from(device)));

    assert_eq!(
        cli::parse_from([
            OsString::from_vec(b"--siz\xff".to_vec()),
            OsString::from("device")
        ]),
        Err(CliError::NonUnicodeOption)
    );
    assert!(matches!(
        cli::parse_from([
            OsString::from("--size"),
            OsString::from_vec(vec![b'1', 0xff]),
            OsString::from("device"),
        ]),
        Err(CliError::InvalidValue {
            option: "--size",
            ..
        })
    ));
}

#[test]
fn error_display_is_useful() {
    let error: Error = CliError::Conflict("--quiet", "--json").into();
    assert_eq!(
        error.to_string(),
        "options '--quiet' and '--json' cannot be used together"
    );
    assert_eq!(
        CliError::MissingOperation.to_string(),
        "select an operation with --size, --info, --check, --backup, or --restore"
    );
}
