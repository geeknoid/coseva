//! Parsing field bytes into Rust scalar types.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::{string::String, vec::Vec};
use core::convert::Infallible;
use core::error::Error as StdError;
use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::str;

#[cfg(feature = "compact_str")]
use compact_str::CompactString;

use crate::error::ErrorKind;

/// Convert a byte slice into a value.
///
/// This is the byte-slice counterpart to [`core::str::FromStr`]: it converts a
/// raw `&[u8]` into a value without requiring the caller to validate UTF-8
/// first. Types that can be produced without an intermediate UTF-8 validation
/// step (integers and floats, for example) implement this directly, avoiding
/// the cost of validating bytes that are then only inspected as ASCII.
///
/// Implementations in this crate report failures through [`ErrorKind`],
/// except for conversions that cannot fail, which use
/// [`core::convert::Infallible`].
///
/// # Examples
///
/// ```
/// use coseva::FromBytes;
///
/// assert_eq!(u32::from_bytes(b"650706"), Ok(650_706));
/// assert_eq!(f64::from_bytes(b"-2.25"), Ok(-2.25));
/// assert_eq!(bool::from_bytes(b"true"), Ok(true));
/// assert!(u32::from_bytes(b"12a").is_err());
/// ```
pub trait FromBytes: Sized {
    /// Error returned when the bytes do not represent a valid value.
    ///
    /// Any error type works, exactly as for [`core::str::FromStr`]. The
    /// bound only lets a parser carry the failure as the source of a
    /// [`crate::Error`]. Implementations with nothing type-specific to
    /// report should use [`ErrorKind`], which every built-in conversion
    /// returns and which a parser records without allocating.
    type Err: StdError + Send + Sync + 'static;

    /// Convert `bytes` into a value.
    ///
    /// # Errors
    ///
    /// Returns [`Self::Err`] when `bytes` does not represent a valid value.
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err>;
}

/// Validate `bytes` as UTF-8, reporting failure as a [`ErrorKind`].
fn as_utf8(bytes: &[u8]) -> Result<&str, ErrorKind> {
    str::from_utf8(bytes).map_err(ErrorKind::InvalidUtf8)
}

/// Convert `bytes` through the target type's [`core::str::FromStr`].
fn from_bytes_via_str<T: str::FromStr>(bytes: &[u8]) -> Result<T, ErrorKind> {
    as_utf8(bytes)?
        .parse()
        .map_err(|_error| ErrorKind::InvalidValue)
}

trait DigitFromU8 {
    fn from_digit(digit: u8) -> Self;
}

impl DigitFromU8 for u8 {
    #[inline]
    fn from_digit(digit: u8) -> Self {
        digit
    }
}

impl DigitFromU8 for i8 {
    #[inline]
    fn from_digit(digit: u8) -> Self {
        digit.cast_signed()
    }
}

macro_rules! impl_digit_from_u8 {
    ($($t:ty),*) => {
        $(
            impl DigitFromU8 for $t {
                #[inline]
                fn from_digit(digit: u8) -> Self {
                    <$t>::from(digit)
                }
            }
        )*
    };
}

impl_digit_from_u8!(u16, u32, u64, u128, usize, i16, i32, i64, i128, isize);

macro_rules! define_unsigned_parser {
    ($name:ident, $ty:ty) => {
        #[inline]
        pub(crate) fn $name(bytes: &[u8]) -> Result<$ty, ErrorKind> {
            /// Digit count that fits this type whatever the digits are: one
            /// less than the width of its maximum.
            const ALWAYS_FITS: usize = <$ty>::MAX.ilog10() as usize;

            let digits = match bytes {
                [b'+', rest @ ..] => rest,
                _ => bytes,
            };
            if digits.is_empty() {
                return Err(ErrorKind::EmptyField);
            }

            let mut value: $ty = 0;

            // A field short enough to fit the type whatever its digits are
            // cannot overflow, so the accumulation drops the range checks the
            // general loop below has to repeat for every digit.
            if digits.len() <= ALWAYS_FITS {
                for &byte in digits {
                    let digit = byte.wrapping_sub(b'0');
                    if digit > 9 {
                        return Err(ErrorKind::InvalidDigit);
                    }
                    let digit = <$ty>::from_digit(digit);
                    value = value * 10 + digit;
                }
                return Ok(value);
            }

            for &byte in digits {
                let digit = byte.wrapping_sub(b'0');
                if digit > 9 {
                    return Err(ErrorKind::InvalidDigit);
                }
                let digit = <$ty>::from_digit(digit);
                value = value
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(digit))
                    .ok_or(ErrorKind::OutOfRange)?;
            }
            Ok(value)
        }
    };
}

macro_rules! define_signed_parser {
    ($name:ident, $ty:ty) => {
        #[inline]
        pub(crate) fn $name(bytes: &[u8]) -> Result<$ty, ErrorKind> {
            /// Digit count that fits this type whatever the digits are, in
            /// either direction: one less than the width of its maximum.
            const ALWAYS_FITS: usize = <$ty>::MAX.ilog10() as usize;

            let (negative, digits) = match bytes {
                [b'-', rest @ ..] => (true, rest),
                [b'+', rest @ ..] => (false, rest),
                _ => (false, bytes),
            };
            if digits.is_empty() {
                return Err(ErrorKind::EmptyField);
            }

            let mut value: $ty = 0;

            // Short enough to fit whatever the digits are, in either
            // direction, so the accumulation needs no range checks.
            if digits.len() <= ALWAYS_FITS {
                for &byte in digits {
                    let digit = byte.wrapping_sub(b'0');
                    if digit > 9 {
                        return Err(ErrorKind::InvalidDigit);
                    }
                    let digit = <$ty>::from_digit(digit);
                    value = if negative {
                        value * 10 - digit
                    } else {
                        value * 10 + digit
                    };
                }
                return Ok(value);
            }

            for &byte in digits {
                let digit = byte.wrapping_sub(b'0');
                if digit > 9 {
                    return Err(ErrorKind::InvalidDigit);
                }
                let digit = <$ty>::from_digit(digit);
                value = value
                    .checked_mul(10)
                    .and_then(|value| {
                        if negative {
                            value.checked_sub(digit)
                        } else {
                            value.checked_add(digit)
                        }
                    })
                    .ok_or(ErrorKind::OutOfRange)?;
            }
            Ok(value)
        }
    };
}

define_signed_parser!(parse_i8, i8);
define_signed_parser!(parse_i16, i16);
define_signed_parser!(parse_i32, i32);
define_signed_parser!(parse_i64, i64);
define_signed_parser!(parse_i128, i128);
define_signed_parser!(parse_isize, isize);
define_unsigned_parser!(parse_u8, u8);
define_unsigned_parser!(parse_u16, u16);
define_unsigned_parser!(parse_u32, u32);
define_unsigned_parser!(parse_u64, u64);
define_unsigned_parser!(parse_u128, u128);
define_unsigned_parser!(parse_usize, usize);

macro_rules! impl_from_bytes_integer {
    ($(($ty:ty, $parse:ident)),+ $(,)?) => {
        $(
            impl FromBytes for $ty {
                type Err = ErrorKind;

                #[inline]
                fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
                    $parse(bytes)
                }
            }
        )+
    };
}

impl_from_bytes_integer!(
    (i8, parse_i8),
    (i16, parse_i16),
    (i32, parse_i32),
    (i64, parse_i64),
    (i128, parse_i128),
    (isize, parse_isize),
    (u8, parse_u8),
    (u16, parse_u16),
    (u32, parse_u32),
    (u64, parse_u64),
    (u128, parse_u128),
    (usize, parse_usize),
);

macro_rules! impl_from_bytes_via_str {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl FromBytes for $ty {
                type Err = ErrorKind;

                #[inline]
                fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
                    from_bytes_via_str(bytes)
                }
            }
        )+
    };
}

impl_from_bytes_via_str!(
    char,
    IpAddr,
    Ipv4Addr,
    Ipv6Addr,
    SocketAddr,
    SocketAddrV4,
    SocketAddrV6,
);

/// Parse a float straight from the field's raw bytes.
///
/// The Eisel-Lemire algorithm consumes bytes directly, so neither UTF-8
/// validation nor an intermediate `&str` is needed. The accepted grammar and
/// the rounding of every input match [`f64::from_str`](core::str::FromStr),
/// including infinities, subnormals, and overflow to infinity.
macro_rules! impl_from_bytes_float {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl FromBytes for $ty {
                type Err = ErrorKind;

                #[inline]
                fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
                    fast_float2::parse(bytes).map_err(|_error| {
                        if bytes.is_empty() {
                            ErrorKind::EmptyField
                        } else {
                            ErrorKind::InvalidValue
                        }
                    })
                }
            }
        )+
    };
}

impl_from_bytes_float!(f32, f64);

/// Accepts `true`, `false`, `1`, and `0`.
///
/// This is deliberately more permissive than [`bool`]'s
/// [`core::str::FromStr`], because `1` and `0` are common CSV encodings.
impl FromBytes for bool {
    type Err = ErrorKind;

    #[inline]
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
        match bytes {
            b"true" | b"1" => Ok(true),
            b"false" | b"0" => Ok(false),
            _ => Err(ErrorKind::InvalidValue),
        }
    }
}

impl FromBytes for String {
    type Err = ErrorKind;

    #[inline]
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
        as_utf8(bytes).map(Self::from)
    }
}

/// Converts exactly like [`String`], but stores a value of 24 bytes or fewer
/// inline, without allocating.
#[cfg(feature = "compact_str")]
impl FromBytes for CompactString {
    type Err = ErrorKind;

    #[inline]
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
        as_utf8(bytes).map(Self::from)
    }
}

impl FromBytes for Vec<u8> {
    type Err = Infallible;

    #[inline]
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
        Ok(bytes.to_vec())
    }
}

/// An empty input converts to `None`; any other input is delegated to `T`.
impl<T: FromBytes> FromBytes for Option<T> {
    type Err = T::Err;

    #[inline]
    fn from_bytes(bytes: &[u8]) -> Result<Self, Self::Err> {
        if bytes.is_empty() {
            return Ok(None);
        }
        T::from_bytes(bytes).map(Some)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_integers_error_cases() {
        assert_eq!(u32::from_bytes(b"12a"), Err(ErrorKind::InvalidDigit));
        assert_eq!(i32::from_bytes(b"-12a"), Err(ErrorKind::InvalidDigit));
        assert_eq!(i32::from_bytes(b"+12a"), Err(ErrorKind::InvalidDigit));
        assert_eq!(u8::from_bytes(b"999"), Err(ErrorKind::OutOfRange));
        assert_eq!(i8::from_bytes(b"-999"), Err(ErrorKind::OutOfRange));
        assert_eq!(u32::from_bytes(b""), Err(ErrorKind::EmptyField));
        assert_eq!(i32::from_bytes(b"-"), Err(ErrorKind::EmptyField));
        assert_eq!(i32::from_bytes(b"+"), Err(ErrorKind::EmptyField));

        assert_eq!(char::from_bytes(b"x"), Ok('x'));
        assert!(char::from_bytes(b"xyz").is_err());
        assert!(char::from_bytes(b"\xff").is_err());
        assert!(String::from_bytes(b"\xff").is_err());
        assert!(Ipv4Addr::from_bytes(b"\xff").is_err());
        assert_eq!(
            IpAddr::from_bytes(b"127.0.0.1"),
            Ok("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            Ipv4Addr::from_bytes(b"127.0.0.1"),
            Ok("127.0.0.1".parse().unwrap())
        );
        assert_eq!(Ipv6Addr::from_bytes(b"::1"), Ok("::1".parse().unwrap()));
        assert_eq!(
            SocketAddr::from_bytes(b"127.0.0.1:8080"),
            Ok("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(
            SocketAddrV4::from_bytes(b"127.0.0.1:8080"),
            Ok("127.0.0.1:8080".parse().unwrap())
        );
        assert_eq!(
            SocketAddrV6::from_bytes(b"[::1]:8080"),
            Ok("[::1]:8080".parse().unwrap())
        );
        assert_eq!(Option::<u32>::from_bytes(b""), Ok(None));
        assert_eq!(Option::<u32>::from_bytes(b"42"), Ok(Some(42)));
        assert_eq!(Vec::<u8>::from_bytes(b"bytes"), Ok(b"bytes".to_vec()));
        assert_eq!(String::from_bytes(b"hello"), Ok("hello".to_string()));
        #[cfg(feature = "compact_str")]
        assert_eq!(
            CompactString::from_bytes(b"hello"),
            Ok(CompactString::from("hello"))
        );

        // Floats and bool
        assert_eq!(f32::from_bytes(b"1.23"), Ok(1.23));
        assert_eq!(f32::from_bytes(b""), Err(ErrorKind::EmptyField));
        assert_eq!(f32::from_bytes(b"abc"), Err(ErrorKind::InvalidValue));
        assert_eq!(f64::from_bytes(b"1.23"), Ok(1.23));
        assert_eq!(f64::from_bytes(b""), Err(ErrorKind::EmptyField));
        assert_eq!(f64::from_bytes(b"abc"), Err(ErrorKind::InvalidValue));
        assert_eq!(bool::from_bytes(b"true"), Ok(true));
        assert_eq!(bool::from_bytes(b"1"), Ok(true));
        assert_eq!(bool::from_bytes(b"false"), Ok(false));
        assert_eq!(bool::from_bytes(b"0"), Ok(false));
        assert_eq!(bool::from_bytes(b"yes"), Err(ErrorKind::InvalidValue));

        // Remaining integer types
        assert_eq!(u16::from_bytes(b"+42"), Ok(42));
        assert_eq!(u16::from_bytes(b"65535"), Ok(65535));
        assert_eq!(u16::from_bytes(b"65536"), Err(ErrorKind::OutOfRange));
        assert_eq!(u16::from_bytes(b"12a"), Err(ErrorKind::InvalidDigit));
        assert_eq!(u64::from_bytes(b"+42"), Ok(42));
        assert_eq!(
            u64::from_bytes(b"18446744073709551616"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(u128::from_bytes(b"+42"), Ok(42));
        assert_eq!(
            u128::from_bytes(b"340282366920938463463374607431768211456"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(usize::from_bytes(b"+42"), Ok(42));
        assert_eq!(
            usize::from_bytes(b"18446744073709551616"),
            Err(ErrorKind::OutOfRange)
        );

        assert_eq!(i16::from_bytes(b"+42"), Ok(42));
        assert_eq!(i16::from_bytes(b"-42"), Ok(-42));
        assert_eq!(i16::from_bytes(b"32768"), Err(ErrorKind::OutOfRange));
        assert_eq!(i16::from_bytes(b"-32769"), Err(ErrorKind::OutOfRange));
        assert_eq!(i64::from_bytes(b"+42"), Ok(42));
        assert_eq!(i64::from_bytes(b"-42"), Ok(-42));
        assert_eq!(
            i64::from_bytes(b"9223372036854775808"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(
            i64::from_bytes(b"-9223372036854775809"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(i128::from_bytes(b"+42"), Ok(42));
        assert_eq!(i128::from_bytes(b"-42"), Ok(-42));
        assert_eq!(
            i128::from_bytes(b"170141183460469231731687303715884105728"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(
            i128::from_bytes(b"-170141183460469231731687303715884105729"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(isize::from_bytes(b"+42"), Ok(42));
        assert_eq!(isize::from_bytes(b"-42"), Ok(-42));
        assert_eq!(
            isize::from_bytes(b"9223372036854775808"),
            Err(ErrorKind::OutOfRange)
        );
        assert_eq!(
            isize::from_bytes(b"-9223372036854775809"),
            Err(ErrorKind::OutOfRange)
        );
    }
}
