use crate::error::{Error, ErrorKind};

pub(super) const DEFAULT_READ_BUFFER_BYTES: usize = 8 * 1024;
pub(super) const DEFAULT_WRITE_BUFFER_BYTES: usize = 8 * 1024;

pub(super) fn validate_buffer_capacity(capacity: usize) -> Result<(), Error> {
    if capacity == 0 {
        return Err(Error::detailed(
            ErrorKind::Configuration,
            "buffer capacity must be greater than zero",
        ));
    }
    if capacity > isize::MAX as usize {
        return Err(Error::detailed(
            ErrorKind::Configuration,
            "buffer capacity exceeds the platform allocation limit",
        ));
    }
    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_enforces_both_capacity_boundaries_with_specific_errors() {
        let zero = validate_buffer_capacity(0).expect_err("zero is unusable");
        assert_eq!(
            zero.to_string(),
            "buffer capacity must be greater than zero"
        );

        validate_buffer_capacity(isize::MAX as usize).expect("isize::MAX is permitted");
        let oversized =
            validate_buffer_capacity(isize::MAX as usize + 1).expect_err("past isize::MAX");
        assert_eq!(
            oversized.to_string(),
            "buffer capacity exceeds the platform allocation limit"
        );
    }
}
