/// Bytes following a separator's lead byte.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Tail {
    bytes: [u8; Self::MAX],
    len: u8,
}

impl Tail {
    /// The longest tail, making the longest separator `MAX + 1` bytes.
    pub(crate) const MAX: usize = 3;

    /// The tail of a single-byte separator.
    pub(crate) const EMPTY: Self = Self {
        bytes: [0; Self::MAX],
        len: 0,
    };

    /// Marks a separator that is empty or longer than [`Self::MAX`] + 1.
    ///
    /// Sequence builders are `const fn` and cannot return an error, so
    /// `Dialect::invalidity` reports this sentinel with other malformed
    /// dialect settings.
    #[cfg(feature = "multibyte")]
    const UNUSABLE: u8 = u8::MAX;

    /// Builds the tail of `bytes`; the scanner searches for the first byte.
    #[cfg(feature = "multibyte")]
    pub(crate) const fn of(bytes: &[u8]) -> Self {
        let mut tail = Self::EMPTY;
        if bytes.is_empty() || bytes.len() > Self::MAX + 1 {
            tail.len = Self::UNUSABLE;
            return tail;
        }
        let mut at = 1;
        while at < bytes.len() {
            tail.bytes[at - 1] = bytes[at];
            at += 1;
        }
        // These are the only valid lengths; spelling them out avoids a
        // truncating cast the compiler cannot prove safe.
        tail.len = match bytes.len() {
            1 => 0,
            2 => 1,
            3 => 2,
            _ => 3,
        };
        tail
    }

    /// Rebuilds a tail from its stored representation.
    #[cfg(feature = "index")]
    pub(crate) const fn from_parts(len: u8, bytes: [u8; Self::MAX]) -> Self {
        Self { bytes, len }
    }

    /// Returns the stored representation.
    #[cfg(feature = "index")]
    pub(crate) const fn parts(self) -> (u8, [u8; Self::MAX]) {
        (self.len, self.bytes)
    }

    /// Whether this belongs to a single-byte separator.
    pub(crate) const fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Whether the stored length is invalid.
    pub(crate) const fn unusable(self) -> bool {
        self.len as usize > Self::MAX
    }

    /// Returns the separator width, including its lead byte.
    pub(crate) const fn width(self) -> usize {
        self.len as usize + 1
    }

    /// Returns the tail bytes in order.
    pub(crate) const fn as_slice(&self) -> &[u8] {
        let len = if self.unusable() {
            0
        } else {
            self.len as usize
        };
        self.bytes.split_at(len).0
    }

    /// Whether `after_lead` begins with this tail.
    #[inline]
    pub(crate) fn confirms(self, after_lead: &[u8]) -> bool {
        let tail = self.as_slice();
        after_lead.len() >= tail.len() && after_lead.split_at(tail.len()).0 == tail
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    #[cfg(any(feature = "multibyte", feature = "index"))]
    use super::*;

    #[test]
    #[cfg(feature = "multibyte")]
    fn tail_methods_multibyte() {
        let t1 = Tail::of(b"a");
        assert_eq!(t1.len, 0);
        assert!(t1.is_empty());
        assert_eq!(t1.width(), 1);
        assert_eq!(t1.as_slice(), b"");
        assert!(t1.confirms(b"abc"));

        let t2 = Tail::of(b"ab");
        assert_eq!(t2.len, 1);
        assert_eq!(t2.width(), 2);
        assert_eq!(t2.as_slice(), b"b");
        assert!(t2.confirms(b"b_more"));
        assert!(!t2.confirms(b"c"));
        assert!(!t2.confirms(b""));

        let t3 = Tail::of(b"abc");
        assert_eq!(t3.len, 2);
        assert_eq!(t3.width(), 3);
        assert_eq!(t3.as_slice(), b"bc");

        let t4 = Tail::of(b"abcd");
        assert_eq!(t4.len, 3);
        assert_eq!(t4.width(), 4);
        assert_eq!(t4.as_slice(), b"bcd");

        let t_too_long = Tail::of(b"abcde");
        assert!(t_too_long.unusable());

        let t_invalid = Tail::of(b"");
        assert!(t_invalid.unusable());
        assert_eq!(t_invalid.as_slice(), b"");
    }

    #[test]
    #[cfg(feature = "index")]
    fn tail_methods_index() {
        let t = Tail::from_parts(2, [1, 2, 0]);
        assert_eq!(t.parts(), (2, [1, 2, 0]));
    }
}
