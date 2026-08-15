#![allow(
    clippy::inline_always,
    reason = "shared field conversions must retain the inlining of their former method bodies"
)]

use core::error::Error as StdError;
use core::str::{self, FromStr};

use crate::FromBytes;
use crate::error::{Error, Location};

#[inline(always)]
pub(crate) fn get_str(field: Option<&[u8]>, index: usize) -> Result<Option<&str>, Error> {
    field
        .map(|field| {
            str::from_utf8(field).map_err(|error| Error::utf8(error, index, Location::UNKNOWN))
        })
        .transpose()
}

#[inline(always)]
pub(crate) fn parse<T: FromBytes>(field: Option<&[u8]>, index: usize) -> Result<Option<T>, Error> {
    let Some(field) = field else {
        return Ok(None);
    };
    T::from_bytes(field)
        .map(Some)
        .map_err(|error| Error::from_conversion(error, Location::UNKNOWN, index))
}

#[inline(always)]
pub(crate) fn parse_from_str<T: FromStr>(
    field: Option<&[u8]>,
    index: usize,
) -> Result<Option<T>, Error>
where
    T::Err: StdError + Send + Sync + 'static,
{
    let Some(field) = field else {
        return Ok(None);
    };
    let value =
        str::from_utf8(field).map_err(|error| Error::utf8(error, index, Location::UNKNOWN))?;
    value
        .parse()
        .map(Some)
        .map_err(|error| Error::from_conversion(error, Location::UNKNOWN, index))
}
