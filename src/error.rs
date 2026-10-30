use std::error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SizeParseError {
    Empty,
    InvalidNumber,
    UnknownSuffix(String),
    FractionalByte,
    Overflow,
}

impl fmt::Display for SizeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "size is empty"),
            Self::InvalidNumber => write!(f, "size must be a non-negative number"),
            Self::UnknownSuffix(suffix) => write!(f, "unknown size suffix '{suffix}'"),
            Self::FractionalByte => write!(f, "size does not resolve to a whole number of bytes"),
            Self::Overflow => write!(f, "size is too large"),
        }
    }
}

impl error::Error for SizeParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    UnknownOption(String),
    MissingValue(&'static str),
    InvalidValue {
        option: &'static str,
        value: String,
        reason: String,
    },
    DuplicateOption(&'static str),
    MissingDevice,
    MissingOperation,
    UnexpectedArgument(String),
    Conflict(&'static str, &'static str),
    NonUnicodeOption,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOption(option) => write!(f, "unknown option '{option}'"),
            Self::MissingValue(option) => write!(f, "option '{option}' requires a value"),
            Self::InvalidValue {
                option,
                value,
                reason,
            } => write!(f, "invalid value '{value}' for '{option}': {reason}"),
            Self::DuplicateOption(option) => {
                write!(f, "option '{option}' was provided more than once")
            }
            Self::MissingDevice => write!(f, "a device path is required"),
            Self::MissingOperation => write!(
                f,
                "select an operation with --size, --info, --check, --backup, or --restore"
            ),
            Self::UnexpectedArgument(argument) => write!(f, "unexpected argument '{argument}'"),
            Self::Conflict(left, right) => {
                write!(f, "options '{left}' and '{right}' cannot be used together")
            }
            Self::NonUnicodeOption => write!(f, "option names and values must be valid UTF-8"),
        }
    }
}

impl error::Error for CliError {}

#[derive(Debug)]
pub enum Error {
    Cli(CliError),
    Io(std::io::Error),
    Resize(crate::resize::ResizeError),
    NotImplemented(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cli(error) => fmt::Display::fmt(error, f),
            Self::Io(error) => fmt::Display::fmt(error, f),
            Self::Resize(error) => fmt::Display::fmt(error, f),
            Self::NotImplemented(operation) => {
                write!(
                    f,
                    "{operation} is not implemented in this foundation release"
                )
            }
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Cli(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Resize(error) => Some(error),
            Self::NotImplemented(_) => None,
        }
    }
}

impl From<CliError> for Error {
    fn from(value: CliError) -> Self {
        Self::Cli(value)
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<crate::resize::ResizeError> for Error {
    fn from(value: crate::resize::ResizeError) -> Self {
        Self::Resize(value)
    }
}
