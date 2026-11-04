use std::fmt;
use std::str::FromStr;

use crate::error::SizeParseError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Size(u64);

impl Size {
    /// Sentinel used by the compatibility `max` token. Operational entry
    /// points resolve it against the selected container's safe capacity.
    pub const MAX: u64 = u64::MAX;

    /// Sentinel used by the `min` token. Operational entry points resolve it
    /// against the smallest geometry that can hold every allocated cluster.
    pub const MIN: u64 = u64::MAX - 1;

    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }

    pub const fn is_max(self) -> bool {
        self.0 == Self::MAX
    }

    pub const fn is_min(self) -> bool {
        self.0 == Self::MIN
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_max() {
            f.write_str("max")
        } else if self.is_min() {
            f.write_str("min")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

impl FromStr for Size {
    type Err = SizeParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.eq_ignore_ascii_case("max") {
            return Ok(Self(Self::MAX));
        }
        if input.eq_ignore_ascii_case("min") {
            return Ok(Self(Self::MIN));
        }
        if input.is_empty() {
            return Err(SizeParseError::Empty);
        }
        if input.trim() != input {
            return Err(SizeParseError::InvalidNumber);
        }
        if input.starts_with('+') || input.starts_with('-') {
            return Err(SizeParseError::InvalidNumber);
        }

        let number_end = input
            .find(|character: char| !character.is_ascii_digit() && character != '.')
            .unwrap_or(input.len());
        let (number, suffix) = input.split_at(number_end);
        let multiplier = suffix_multiplier(suffix)?;
        let (whole, fraction) = match number.split_once('.') {
            Some((whole, fraction)) if !whole.is_empty() && !fraction.is_empty() => {
                if fraction.contains('.') {
                    return Err(SizeParseError::InvalidNumber);
                }
                (whole, Some(fraction))
            }
            Some(_) => return Err(SizeParseError::InvalidNumber),
            None if !number.is_empty() => (number, None),
            None => return Err(SizeParseError::InvalidNumber),
        };

        let whole = whole.parse::<u64>().map_err(|_| SizeParseError::Overflow)?;
        let mut bytes = whole
            .checked_mul(multiplier)
            .ok_or(SizeParseError::Overflow)?;

        if let Some(fraction) = fraction {
            if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(SizeParseError::InvalidNumber);
            }
            let numerator = fraction
                .parse::<u64>()
                .map_err(|_| SizeParseError::Overflow)?;
            let denominator = checked_power_of_ten(fraction.len())?;
            let scaled = numerator
                .checked_mul(multiplier)
                .ok_or(SizeParseError::Overflow)?;
            if scaled % denominator != 0 {
                return Err(SizeParseError::FractionalByte);
            }
            bytes = bytes
                .checked_add(scaled / denominator)
                .ok_or(SizeParseError::Overflow)?;
        }

        Ok(Self(bytes))
    }
}

fn checked_power_of_ten(exponent: usize) -> Result<u64, SizeParseError> {
    (0..exponent).try_fold(1_u64, |value, _| {
        value.checked_mul(10).ok_or(SizeParseError::Overflow)
    })
}

fn suffix_multiplier(suffix: &str) -> Result<u64, SizeParseError> {
    let normalized = suffix.to_ascii_lowercase();
    let power = match normalized.as_str() {
        "" | "b" => 0,
        "k" | "kb" | "ki" | "kib" => 1,
        "m" | "mb" | "mi" | "mib" => 2,
        "g" | "gb" | "gi" | "gib" => 3,
        "t" | "tb" | "ti" | "tib" => 4,
        "p" | "pb" | "pi" | "pib" => 5,
        _ => return Err(SizeParseError::UnknownSuffix(suffix.to_owned())),
    };
    (0..power).try_fold(1_u64, |value, _| {
        value.checked_mul(1024).ok_or(SizeParseError::Overflow)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bytes_and_binary_suffixes() {
        assert_eq!("0".parse::<Size>().unwrap().bytes(), 0);
        assert_eq!("4096B".parse::<Size>().unwrap().bytes(), 4096);
        assert_eq!("2K".parse::<Size>().unwrap().bytes(), 2 * 1024);
        assert_eq!("2Mi".parse::<Size>().unwrap().bytes(), 2 * 1024 * 1024);
        assert_eq!("3MiB".parse::<Size>().unwrap().bytes(), 3 * 1024 * 1024);
        assert_eq!("1g".parse::<Size>().unwrap().bytes(), 1024 * 1024 * 1024);
        assert_eq!("max".parse::<Size>().unwrap().bytes(), Size::MAX);
        assert_eq!("MAX".parse::<Size>().unwrap().bytes(), Size::MAX);
        assert_eq!("max".parse::<Size>().unwrap().to_string(), "max");
        assert_eq!("min".parse::<Size>().unwrap().bytes(), Size::MIN);
        assert_eq!("MiN".parse::<Size>().unwrap().bytes(), Size::MIN);
        assert_eq!("min".parse::<Size>().unwrap().to_string(), "min");
    }

    #[test]
    fn parses_exact_fractional_units() {
        assert_eq!("1.5KiB".parse::<Size>().unwrap().bytes(), 1536);
        assert_eq!("0.5K".parse::<Size>().unwrap().bytes(), 512);
    }

    #[test]
    fn rejects_invalid_sizes() {
        assert_eq!("".parse::<Size>(), Err(SizeParseError::Empty));
        assert_eq!("-1K".parse::<Size>(), Err(SizeParseError::InvalidNumber));
        assert_eq!("1.1B".parse::<Size>(), Err(SizeParseError::FractionalByte));
        assert_eq!(
            "12XB".parse::<Size>(),
            Err(SizeParseError::UnknownSuffix("XB".to_owned()))
        );
        assert_eq!("1K ".parse::<Size>(), Err(SizeParseError::InvalidNumber));
    }

    #[test]
    fn rejects_overflow() {
        assert_eq!(
            "18446744073709551616".parse::<Size>(),
            Err(SizeParseError::Overflow)
        );
        assert_eq!(
            "18446744073709551615K".parse::<Size>(),
            Err(SizeParseError::Overflow)
        );
    }
}
