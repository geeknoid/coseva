//! Bounded raw-byte operations.

use alloc::vec::Vec;
use core::str;

/// Append a field to the byte buffer.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "callers specialize the checked short-write adapter by field length"
)]
pub fn append_short(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(field);
}

/// Borrow validated UTF-8.
#[inline]
pub fn borrow_utf8(bytes: &[u8]) -> Result<&str, str::Utf8Error> {
    str::from_utf8(bytes)
}

#[inline]
pub fn is_ascii(bytes: &[u8]) -> bool {
    bytes.is_ascii()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{append_short, borrow_utf8, is_ascii};
    use alloc::vec;
    use alloc::vec::Vec;

    #[test]
    fn bytes_helpers() {
        let mut v = Vec::new();
        append_short(&mut v, b"");
        append_short(&mut v, b"a");
        append_short(&mut v, b"ab");
        append_short(&mut v, b"abc");
        append_short(&mut v, b"longfield");
        assert_eq!(&v, b"aababclongfield");

        assert_eq!(borrow_utf8(b"ascii text"), Ok("ascii text"));
        assert_eq!(borrow_utf8("héllo".as_bytes()), Ok("héllo"));
        assert!(borrow_utf8(b"\xff\xff").is_err());
        assert!(is_ascii(b"short"));
        assert!(!is_ascii(b"\x80"));
        assert!(is_ascii(&[b'a'; 32]));
        assert!(!is_ascii(&[0xff; 32]));
    }

    #[test]
    fn short_append_preserves_every_length_boundary_and_prefix() {
        for len in 0..=8 {
            let field: Vec<u8> = (0..len).map(|index| b'a' + index as u8).collect();
            let mut actual = b"prefix".to_vec();
            let mut expected = actual.clone();
            expected.extend_from_slice(&field);
            append_short(&mut actual, &field);
            assert_eq!(actual, expected, "field length {len}");
        }
    }

    #[test]
    fn utf8_borrowing_matches_checked_validation() {
        for bytes in [
            b"".as_slice(),
            b"plain ascii",
            "é".as_bytes(),
            "a😀z".as_bytes(),
            b"\x80",
            b"\xc2",
            b"\xe2\x82",
            b"\xf4\x90\x80\x80",
        ] {
            assert_eq!(borrow_utf8(bytes), str::from_utf8(bytes), "{bytes:?}");
        }
    }

    #[test]
    fn ascii_scan_matches_the_standard_library_at_word_boundaries() {
        for len in 0..=65 {
            for fill in [0, b'a'] {
                let bytes = vec![fill; len];
                assert_eq!(is_ascii(&bytes), bytes.is_ascii(), "len={len}, fill={fill}");

                for position in 0..len {
                    let mut bytes = bytes.clone();
                    bytes[position] = 0x80;
                    assert_eq!(
                        is_ascii(&bytes),
                        bytes.is_ascii(),
                        "len={len}, fill={fill}, non_ascii={position}"
                    );
                }
            }
        }
    }
}
