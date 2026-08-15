//! Byte-search and structural-mask kernels.

#[cfg(target_arch = "x86")]
use core::arch::x86::{
    __m256i, _mm256_cmpeq_epi8, _mm256_movemask_epi8, _mm256_or_si256, _mm256_set1_epi8,
    _mm256_setr_epi8,
};
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::{
    __m256i, _mm256_cmpeq_epi8, _mm256_movemask_epi8, _mm256_or_si256, _mm256_set1_epi8,
    _mm256_setr_epi8, _pdep_u64, _pext_u64,
};
#[cfg(target_arch = "x86_64")]
use core::mem;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
const AVX2_BLOCK_BYTES: usize = 32;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
const AVX2_FIND_MIN_BYTES: usize = 128;
const TWO_WAY_MIN_NEEDLE_BYTES: usize = 8;
#[cfg(target_arch = "aarch64")]
const NEON_BLOCK_BYTES: usize = 32;
/// One bit per byte fills the `u32` mask and matches one AVX2 or two NEON vectors.
const STRUCTURAL_BLOCK_BYTES: usize = 32;

/// Builds a SIMD vector by listing every lane of `block` in ascending order.
///
/// The `setr`-flavoured intrinsics take their lanes in memory order and accept
/// plain integers rather than a pointer, so a whole block can be loaded without
/// `unsafe`. LLVM recognises the pattern and folds it back into the single
/// unaligned load it would have emitted for `_mm256_loadu_si256`, so this costs
/// nothing; it merely removes the raw pointer and the readability proof that
/// came with it.
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
macro_rules! lanes {
    ($intrinsic:path, $block:expr, $($index:literal),+ $(,)?) => {
        $intrinsic($($block[$index].cast_signed()),+)
    };
}

#[derive(Clone, Copy, Debug)]
pub struct StructuralBlock<'input> {
    input: &'input [u8],
    start: usize,
    mask: u32,
}

impl StructuralBlock<'_> {
    // gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
    #[inline]
    pub fn next_offset(&mut self) -> Option<usize> {
        if self.mask == 0 {
            // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
            return None;
        }
        let relative = self.mask.trailing_zeros() as usize;
        // gamma::skip(assign.and_to_or, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(arith.sub_to_add, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(arith.sub_to_div, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
        self.mask &= self.mask - 1;
        Some(self.start + relative)
    }

    // gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
    /// Return the next structural offset and the byte at that offset.
    #[inline]
    pub fn next_match(&mut self) -> Option<(usize, u8)> {
        if self.mask == 0 {
            // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
            return None;
        }
        let relative = self.mask.trailing_zeros() as usize;
        // gamma::skip(assign.and_to_or, reason = "mutation causes non-termination or unbounded resource use")
        self.mask &= self.mask - 1;
        // SAFETY: every set mask bit was computed from a byte in `input`.
        let byte = unsafe { *self.input.get_unchecked(self.start + relative) };
        Some((self.start + relative, byte))
    }

    #[cfg(feature = "benchmarking")]
    pub const fn count(&self) -> usize {
        self.mask.count_ones() as usize
    }

    #[cfg(any(test, not(target_arch = "aarch64")))]
    const fn matches(&self) -> usize {
        self.mask.count_ones() as usize
    }
}

/// Compatibility token for resuming a structural scan.
///
/// Resumed scans recompute masks from their input, so the token intentionally
/// carries no data.
#[derive(Clone, Copy, Debug)]
pub struct BlockCache;

impl BlockCache {
    pub const fn new() -> Self {
        Self
    }
}

#[derive(Debug)]
pub struct StructuralBlocks<'input> {
    input: &'input [u8],
    first: u8,
    second: u8,
    third: u8,
    next_block: usize,
    skip_below: usize,
}

impl<'input> StructuralBlocks<'input> {
    pub fn new(input: &'input [u8], first: u8, second: u8, third: u8) -> Self {
        Self {
            input,
            first,
            second,
            third,
            next_block: 0,
            skip_below: 0,
        }
    }

    /// Scans `input` from `position`, anchoring the block grid to `input[0]`.
    ///
    /// Because the grid is anchored to the whole input rather than to
    /// `position`, consecutive scans see the same block boundaries. `cache`
    /// preserves the scanner checkpoint API but is not trusted across slices.
    /// Yielded offsets are relative to `input[0]`, not to `position`.
    pub fn resume(
        input: &'input [u8],
        first: u8,
        second: u8,
        third: u8,
        position: usize,
        _cache: BlockCache,
    ) -> Self {
        Self {
            input,
            first,
            second,
            third,
            next_block: position & !(STRUCTURAL_BLOCK_BYTES - 1),
            skip_below: position,
        }
    }

    /// Returns the compatibility token accepted by [`Self::resume`].
    pub const fn cache(&self) -> BlockCache {
        BlockCache
    }

    #[inline]
    fn block_mask(&self, remaining: &[u8], block_len: usize) -> u32 {
        if block_len == STRUCTURAL_BLOCK_BYTES
            && let Some((block, _)) = remaining.split_first_chunk::<STRUCTURAL_BLOCK_BYTES>()
        {
            #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
            if avx2_available() {
                // SAFETY: Runtime detection proved AVX2 support.
                return unsafe { block_mask_avx2(block, self.first, self.second, self.third) };
            }
            #[cfg(target_arch = "aarch64")]
            {
                return block_mask_neon::<3>(block, self.first, self.second, self.third, 0);
            }
        }
        scalar_block_mask(&remaining[..block_len], self.first, self.second, self.third)
    }
}

impl<'input> Iterator for StructuralBlocks<'input> {
    type Item = StructuralBlock<'input>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while
        // gamma::skip(relational.lt_to_le, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        self.next_block < self.input.len() {
            let start = self.next_block;
            // gamma::skip(expr.increment, reason = "mutation causes non-termination or unbounded resource use")
            let remaining = &self.input[start..];
            let block_len = remaining.len().min(STRUCTURAL_BLOCK_BYTES);
            let mut mask = self.block_mask(remaining, block_len);
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.next_block = start + block_len;
            let skipped = self.skip_below.saturating_sub(start);
            debug_assert!(
                skipped < STRUCTURAL_BLOCK_BYTES,
                "skip distance is not inside the block, so the mask shift below \
                 would overflow"
            );
            mask &= u32::MAX << skipped;
            if mask != 0 {
                return Some(StructuralBlock {
                    input: self.input,
                    start,
                    mask,
                });
            }
        }
        None
    }
}

#[inline]
fn scalar_block_mask(input: &[u8], first: u8, second: u8, third: u8) -> u32 {
    debug_assert!(
        input.len() <= STRUCTURAL_BLOCK_BYTES,
        "block is wider than the mask has bits, so trailing bytes would be \
         silently dropped from the scan"
    );
    input.iter().enumerate().fold(0, |mask, (index, &byte)| {
        mask | (u32::from(byte == first || byte == second || byte == third) << index)
    })
}

#[inline]
pub fn find1(first: u8, input: &[u8]) -> Option<usize> {
    find(&[first], input)
}

/// Find the first occurrence of a multi-byte literal.
///
/// An empty needle matches at offset zero; a one-byte needle uses [`find1`].
/// Longer needles use worst-case-linear two-way matching with a SIMD scan on
/// the final needle byte. This also backs `Predicate::contains` in the facade
/// crate on already-decoded field values, so raw pushdown and decoded
/// containment share one implementation instead of each carrying their own.
pub fn find_literal(needle: &[u8], input: &[u8]) -> Option<usize> {
    match needle {
        [] => Some(0),
        &[first] => find1(first, input),
        _ if needle.len() > input.len() => None,
        _ if needle.len() < TWO_WAY_MIN_NEEDLE_BYTES => input
            .windows(needle.len())
            .position(|candidate| candidate == needle),
        _ => two_way_find(needle, input),
    }
}

fn two_way_find(needle: &[u8], input: &[u8]) -> Option<usize> {
    debug_assert!(needle.len() >= 2);
    debug_assert!(input.len() >= needle.len());

    let ascending_suffix = maximal_suffix(needle, false);
    let descending_suffix = maximal_suffix(needle, true);
    let (crit_pos, crit_period) = if ascending_suffix.0 > descending_suffix.0 {
        ascending_suffix
    } else {
        descending_suffix
    };

    let short_period = needle[..crit_pos] == needle[crit_period..crit_period + crit_pos];
    let period = if short_period {
        crit_period
    } else {
        crit_pos.max(needle.len() - crit_pos) + 1
    };

    let anchor = needle.len() - 1;
    let last_start = input.len() - needle.len();
    let mut base = 0;
    let mut memory = 0;

    loop {
        if base > last_start {
            return None;
        }

        let skip = find_literal_anchor(needle[anchor], &input[base + anchor..])?;
        if skip > 0 {
            memory = 0;
        }
        base += skip;

        let mut i = if short_period {
            crit_pos.max(memory)
        } else {
            crit_pos
        };
        while i < needle.len() && needle[i] == input[base + i] {
            i += 1;
        }
        if i < needle.len() {
            base += i - crit_pos + 1;
            if short_period {
                memory = 0;
            }
            continue;
        }

        let back_limit = if short_period { memory } else { 0 };
        let mut mismatch = false;
        let mut j = crit_pos;
        while j > back_limit {
            j -= 1;
            if needle[j] != input[base + j] {
                mismatch = true;
                break;
            }
        }
        if !mismatch {
            return Some(base);
        }
        base += period;
        if short_period {
            memory = needle.len() - period;
        }
    }
}

fn maximal_suffix(needle: &[u8], reverse_order: bool) -> (usize, usize) {
    let mut pos = 0;
    let mut candidate = 1;
    let mut offset = 0;
    let mut period = 1;

    while let Some(&next) = needle.get(candidate + offset) {
        let current = needle[pos + offset];
        let candidate_is_smaller = if reverse_order {
            next > current
        } else {
            next < current
        };
        if candidate_is_smaller {
            candidate += offset + 1;
            offset = 0;
            period = candidate - pos;
        } else if next == current {
            if offset + 1 == period {
                candidate += offset + 1;
                offset = 0;
            } else {
                offset += 1;
            }
        } else {
            pos = candidate;
            candidate += 1;
            offset = 0;
            period = 1;
        }
    }
    (pos, period)
}

#[inline]
fn find_literal_anchor(needle: u8, input: &[u8]) -> Option<usize> {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if input.len() >= AVX2_FIND_MIN_BYTES && avx2_available() {
        // SAFETY: Runtime detection proved AVX2 support.
        return unsafe { find_avx2::<1>(input, needle, 0, 0, 0) };
    }
    #[cfg(target_arch = "aarch64")]
    if input.len() >= NEON_BLOCK_BYTES {
        return find_neon::<1>(input, needle, 0, 0, 0);
    }
    input.iter().position(|&byte| byte == needle)
}

/// Find the last occurrence of `needle` in `input`.
///
/// Used to locate the start of the record containing a candidate hit.
pub fn rfind1(needle: u8, input: &[u8]) -> Option<usize> {
    input.iter().rposition(|&byte| byte == needle)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[inline]
pub fn count1(needle: u8, input: &[u8]) -> usize {
    if input.len() < STRUCTURAL_BLOCK_BYTES {
        return count1_scalar(needle, input);
    }
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if avx2_available() {
        // SAFETY: Runtime detection proved AVX2 support.
        return unsafe { count1_avx2(needle, input) };
    }
    #[cfg(target_arch = "aarch64")]
    return count1_neon(needle, input);
    #[cfg(not(target_arch = "aarch64"))]
    return count1_portable(needle, input);
}

/// Count occurrences using only a portable scalar scan.
///
/// This is what runs on a target without a dedicated counting kernel. It is
/// factored out to stay directly reachable from a test on every host.
#[cfg(any(test, not(target_arch = "aarch64")))]
fn count1_portable(needle: u8, input: &[u8]) -> usize {
    StructuralBlocks::new(input, needle, needle, needle)
        .map(|block| block.matches())
        .sum()
}

#[cfg(target_arch = "aarch64")]
fn count1_neon(needle: u8, input: &[u8]) -> usize {
    let (blocks, tail) = input.as_chunks::<NEON_BLOCK_BYTES>();
    let count = blocks.iter().fold(0, |count, block| {
        count + block_mask_neon::<1>(block, needle, 0, 0, 0).count_ones() as usize
    });
    count + count1_scalar(needle, tail)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[inline]
#[target_feature(enable = "avx2")]
fn load_avx2(block: &[u8; AVX2_BLOCK_BYTES]) -> __m256i {
    lanes!(
        _mm256_setr_epi8,
        block,
        0,
        1,
        2,
        3,
        4,
        5,
        6,
        7,
        8,
        9,
        10,
        11,
        12,
        13,
        14,
        15,
        16,
        17,
        18,
        19,
        20,
        21,
        22,
        23,
        24,
        25,
        26,
        27,
        28,
        29,
        30,
        31,
    )
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
fn count1_avx2(needle: u8, input: &[u8]) -> usize {
    let needle_vector = _mm256_set1_epi8(needle.cast_signed());
    let (blocks, tail) = input.as_chunks::<AVX2_BLOCK_BYTES>();
    let count = blocks.iter().fold(0, |count, block| {
        let block = load_avx2(block);
        count
            + _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, needle_vector))
                .cast_unsigned()
                .count_ones() as usize
    });
    count + count1_scalar(needle, tail)
}

fn count1_scalar(needle: u8, input: &[u8]) -> usize {
    let mut count = 0;
    let mut offset = 0;
    while offset < input.len() {
        count += usize::from(input[offset] == needle);
        offset += 1;
    }
    count
}

#[inline]
pub fn find2(first: u8, second: u8, input: &[u8]) -> Option<usize> {
    find(&[first, second], input)
}

/// Locates the first byte equal to any of three needles, without a scalar prefix.
///
/// The emitter's quoting test uses this: a field is scanned end to end and the
/// overwhelmingly common answer is "no match anywhere", so the scalar prefix
/// that pays off when a hit is expected within a few bytes is pure overhead.
#[inline]
pub fn find3(first: u8, second: u8, third: u8, input: &[u8]) -> Option<usize> {
    find(&[first, second, third], input)
}

/// Locates the first byte equal to any of four needles, without a scalar prefix.
///
/// The newline-terminated dialects need the fourth needle for `\r`; see
/// [`find3`] for why the scalar prefix is omitted.
#[inline]
pub fn find4(first: u8, second: u8, third: u8, fourth: u8, input: &[u8]) -> Option<usize> {
    find(&[first, second, third, fourth], input)
}

#[inline]
pub fn find1_near(first: u8, input: &[u8]) -> Option<usize> {
    find(&[first], input)
}

#[inline]
pub fn find2_near(first: u8, second: u8, input: &[u8]) -> Option<usize> {
    find(&[first, second], input)
}

#[inline]
pub fn find3_near(first: u8, second: u8, third: u8, input: &[u8]) -> Option<usize> {
    find(&[first, second, third], input)
}

/// Locates the first byte equal to any of four needles.
///
/// The general parsing kernel needs this: a `CrLf` dialect must stop on the
/// delimiter, the quote, `\n` and `\r`, and a `MySQL` dialect on the
/// delimiter, the quote, the record ending and `\`. Folding the extra needle
/// into the one comparison chain replaces a second full pass over the field.
#[inline]
pub fn find4_near(first: u8, second: u8, third: u8, fourth: u8, input: &[u8]) -> Option<usize> {
    find(&[first, second, third, fourth], input)
}

#[inline]
fn find(needles: &[u8], input: &[u8]) -> Option<usize> {
    input.iter().position(|byte| needles.contains(byte))
}

#[inline]
const fn matches<const NEEDLES: usize>(
    byte: u8,
    first: u8,
    second: u8,
    third: u8,
    fourth: u8,
) -> bool {
    byte == first
        || (NEEDLES >= 2 && byte == second)
        || (NEEDLES >= 3 && byte == third)
        || (NEEDLES >= 4 && byte == fourth)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub static AVX2_AVAILABLE: fn() -> bool = || {
    #[cfg(feature = "std")]
    {
        std::is_x86_feature_detected!("avx2")
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(target_feature = "avx2")
    }
};
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub use AVX2_AVAILABLE as avx2_available;

#[cfg(target_arch = "x86_64")]
pub static BMI2_AVAILABLE: fn() -> bool = || {
    #[cfg(feature = "std")]
    {
        std::is_x86_feature_detected!("bmi2")
    }
    #[cfg(not(feature = "std"))]
    {
        cfg!(target_feature = "bmi2")
    }
};
#[cfg(target_arch = "x86_64")]
pub use BMI2_AVAILABLE as bmi2_available;

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
pub(crate) fn csv_masks_avx2(input: &[u8; AVX2_BLOCK_BYTES]) -> (u32, u32, u32) {
    let block = load_avx2(input);
    let delimiter = _mm256_set1_epi8(b','.cast_signed());
    let quote = _mm256_set1_epi8(b'"'.cast_signed());
    let newline = _mm256_set1_epi8(b'\n'.cast_signed());
    (
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, delimiter)).cast_unsigned(),
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, quote)).cast_unsigned(),
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, newline)).cast_unsigned(),
    )
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
pub(crate) fn csv_masks_ascii_avx2(input: &[u8; AVX2_BLOCK_BYTES]) -> (u32, u32, u32, bool) {
    let block = load_avx2(input);
    let delimiter = _mm256_set1_epi8(b','.cast_signed());
    let quote = _mm256_set1_epi8(b'"'.cast_signed());
    let newline = _mm256_set1_epi8(b'\n'.cast_signed());
    (
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, delimiter)).cast_unsigned(),
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, quote)).cast_unsigned(),
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, newline)).cast_unsigned(),
        _mm256_movemask_epi8(block) == 0,
    )
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub(crate) fn csv_masks_words_avx2(input: &[u8; AVX2_BLOCK_BYTES]) -> (u32, u32, u32, [u64; 4]) {
    let block = load_avx2(input);
    let delimiter = _mm256_set1_epi8(b','.cast_signed());
    let quote = _mm256_set1_epi8(b'"'.cast_signed());
    let newline = _mm256_set1_epi8(b'\n'.cast_signed());
    // SAFETY: `__m256i` and four `u64` lanes are both exactly 256 bits, and
    // every bit pattern is valid for either representation.
    let words = unsafe { mem::transmute::<__m256i, [u64; 4]>(block) };
    (
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, delimiter)).cast_unsigned(),
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, quote)).cast_unsigned(),
        _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, newline)).cast_unsigned(),
        words,
    )
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "bmi2")]
pub(crate) fn pack_bytes_bmi2(word: u64, removed: u8) -> (u64, u8) {
    let kept = !removed;
    let byte_bits = _pdep_u64(u64::from(kept), 0x0101_0101_0101_0101).wrapping_mul(0xff);
    (
        _pext_u64(word, byte_bits),
        kept.count_ones().to_le_bytes()[0],
    )
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
fn find_avx2<const NEEDLES: usize>(
    input: &[u8],
    first: u8,
    second: u8,
    third: u8,
    fourth: u8,
) -> Option<usize> {
    let first_vector = _mm256_set1_epi8(first.cast_signed());
    let second_vector = _mm256_set1_epi8(second.cast_signed());
    let third_vector = _mm256_set1_epi8(third.cast_signed());
    let fourth_vector = _mm256_set1_epi8(fourth.cast_signed());
    let (blocks, tail) = input.as_chunks::<AVX2_BLOCK_BYTES>();
    for (index, block) in blocks.iter().enumerate() {
        let block = load_avx2(block);
        let mut found = _mm256_cmpeq_epi8(block, first_vector);
        if NEEDLES >= 2 {
            found = _mm256_or_si256(found, _mm256_cmpeq_epi8(block, second_vector));
        }
        if NEEDLES >= 3 {
            found = _mm256_or_si256(found, _mm256_cmpeq_epi8(block, third_vector));
        }
        if NEEDLES >= 4 {
            found = _mm256_or_si256(found, _mm256_cmpeq_epi8(block, fourth_vector));
        }
        let mask = _mm256_movemask_epi8(found).cast_unsigned();
        if mask != 0 {
            return Some(index * AVX2_BLOCK_BYTES + mask.trailing_zeros() as usize);
        }
    }
    let scanned = blocks.len() * AVX2_BLOCK_BYTES;
    tail.iter()
        .position(|&byte| matches::<NEEDLES>(byte, first, second, third, fourth))
        .map(|relative| scanned + relative)
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
fn block_mask_avx2(input: &[u8; STRUCTURAL_BLOCK_BYTES], first: u8, second: u8, third: u8) -> u32 {
    let block = load_avx2(input);
    let first = _mm256_set1_epi8(first.cast_signed());
    let second = _mm256_set1_epi8(second.cast_signed());
    let third = _mm256_set1_epi8(third.cast_signed());
    let found = _mm256_or_si256(
        _mm256_or_si256(
            _mm256_cmpeq_epi8(block, first),
            _mm256_cmpeq_epi8(block, second),
        ),
        _mm256_cmpeq_epi8(block, third),
    );
    _mm256_movemask_epi8(found).cast_unsigned()
}

#[cfg(target_arch = "aarch64")]
#[inline]
fn find_neon<const NEEDLES: usize>(
    input: &[u8],
    first: u8,
    second: u8,
    third: u8,
    fourth: u8,
) -> Option<usize> {
    let (blocks, tail) = input.as_chunks::<NEON_BLOCK_BYTES>();
    for (index, block) in blocks.iter().enumerate() {
        let mask = block_mask_neon::<NEEDLES>(block, first, second, third, fourth);
        if mask != 0 {
            return Some(index * NEON_BLOCK_BYTES + mask.trailing_zeros() as usize);
        }
    }
    let scanned = blocks.len() * NEON_BLOCK_BYTES;
    tail.iter()
        .position(|&byte| matches::<NEEDLES>(byte, first, second, third, fourth))
        .map(|relative| scanned + relative)
}

/// Loads the 16 bytes of `block` starting at `offset` into a NEON vector.
///
/// Unlike x86, NEON has no pointer-free full-width load (`vld1q_u8` takes a
/// raw pointer; the alternatives cost an extra instruction or a target
/// feature), so `unsafe` is kept here but reduced to the trivial case: `block`
/// is a fixed-size array and `offset` always leaves at least 16 bytes.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "measured on x86: block loads must fold into the record parser to keep it inlinable"
)]
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "the offset arithmetic and the load share the single justification below"
)]
fn load_neon(
    block: &[u8; STRUCTURAL_BLOCK_BYTES],
    offset: usize,
) -> core::arch::aarch64::uint8x16_t {
    debug_assert!(
        offset + 16 <= STRUCTURAL_BLOCK_BYTES,
        "vector load would read past the end of the block"
    );
    // SAFETY: Advanced SIMD is part of the `AArch64` baseline, so the intrinsic
    // is always available, and `offset + 16 <= 32` keeps the read inside the
    // fixed-size array.
    unsafe { core::arch::aarch64::vld1q_u8(block.as_ptr().add(offset)) }
}

/// Computes the structural bitmask for one 32-byte block using NEON.
///
/// As with [`block_mask_sse2`], the `#[target_feature(enable = "neon")]`
/// attribute is deliberately omitted so that this stays inlinable into the
/// record parser: Advanced SIMD is part of the `AArch64` baseline, so the
/// attribute would add no portability while blocking inlining.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn block_mask_neon<const NEEDLES: usize>(
    input: &[u8; STRUCTURAL_BLOCK_BYTES],
    first: u8,
    second: u8,
    third: u8,
    fourth: u8,
) -> u32 {
    use core::arch::aarch64::{vceqq_u8, vdupq_n_u8, vorrq_u8};

    let low = load_neon(input, 0);
    let high = load_neon(input, 16);
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "every operation is a NEON intrinsic justified by the single baseline-availability argument below"
    )]
    // SAFETY: Advanced SIMD is part of the `AArch64` baseline, so every
    // intrinsic below is always available on this target.
    unsafe {
        let first = vdupq_n_u8(first);
        let second = vdupq_n_u8(second);
        let third = vdupq_n_u8(third);
        let fourth = vdupq_n_u8(fourth);
        let mut found_low = vceqq_u8(low, first);
        let mut found_high = vceqq_u8(high, first);
        if NEEDLES >= 2 {
            found_low = vorrq_u8(found_low, vceqq_u8(low, second));
            found_high = vorrq_u8(found_high, vceqq_u8(high, second));
        }
        if NEEDLES >= 3 {
            found_low = vorrq_u8(found_low, vceqq_u8(low, third));
            found_high = vorrq_u8(found_high, vceqq_u8(high, third));
        }
        if NEEDLES >= 4 {
            found_low = vorrq_u8(found_low, vceqq_u8(low, fourth));
            found_high = vorrq_u8(found_high, vceqq_u8(high, fourth));
        }
        u32::from(neon_half_mask(found_low)) | (u32::from(neon_half_mask(found_high)) << 16)
    }
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn neon_half_mask(found: core::arch::aarch64::uint8x16_t) -> u16 {
    use core::arch::aarch64::{vaddv_u8, vandq_u8, vget_high_u8, vget_low_u8};

    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "every operation is a NEON intrinsic justified by the single baseline-availability argument below"
    )]
    // SAFETY: Advanced SIMD is part of the `AArch64` baseline, and `WEIGHTS`
    // holds exactly the 16 bytes the load reads.
    unsafe {
        let weights = core::arch::aarch64::vld1q_u8(WEIGHTS.as_ptr());
        let selected = vandq_u8(found, weights);
        u16::from(vaddv_u8(vget_low_u8(selected)))
            | (u16::from(vaddv_u8(vget_high_u8(selected))) << 8)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::{
        BlockCache, STRUCTURAL_BLOCK_BYTES, StructuralBlock, StructuralBlocks, count1,
        count1_portable, find_literal, find1, find1_near, find2, find2_near, find3, find3_near,
        find4, find4_near, rfind1,
    };

    /// Deterministic xorshift generator so the differential tests are
    /// reproducible without pulling in a dependency.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn byte_from(&mut self, alphabet: &[u8]) -> u8 {
            let slot = usize::try_from(self.next_u64() % alphabet.len() as u64)
                .expect("a value reduced modulo the length fits usize");
            alphabet[slot]
        }
    }

    #[expect(
        clippy::naive_bytecount,
        reason = "this is the trivial reference the SIMD count is differentially checked against"
    )]
    fn naive_count1(needle: u8, input: &[u8]) -> usize {
        input.iter().filter(|&&byte| byte == needle).count()
    }

    fn naive_find1(needle: u8, input: &[u8]) -> Option<usize> {
        input.iter().position(|&byte| byte == needle)
    }

    fn naive_find_literal(needle: &[u8], input: &[u8]) -> Option<usize> {
        if needle.is_empty() {
            return Some(0);
        }
        if needle.len() > input.len() {
            return None;
        }
        (0..=input.len() - needle.len()).find(|&start| input[start..].starts_with(needle))
    }

    /// Lengths clustered around the 32-byte block boundary catch tail
    /// off-by-ones, which are the classic scanning-primitive defect.
    const INTERESTING_LENGTHS: &[usize] = &[
        0, 1, 2, 7, 8, 9, 15, 16, 17, 31, 32, 33, 47, 48, 49, 63, 64, 65, 95, 96, 97,
    ];

    #[test]
    fn primitives_match_naive_over_random_inputs() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        // A small alphabet makes the needles land frequently, including in the
        // partial trailing block that the tail path handles separately.
        let alphabet = b"ab\n,\"";
        for &len in INTERESTING_LENGTHS {
            for _ in 0..64 {
                let input: Vec<u8> = (0..len).map(|_| rng.byte_from(alphabet)).collect();
                for &needle in b"ab\n,\"z" {
                    assert_eq!(
                        count1(needle, &input),
                        naive_count1(needle, &input),
                        "count1 needle {needle} len {len} input {input:?}"
                    );
                    assert_eq!(
                        count1_portable(needle, &input),
                        naive_count1(needle, &input),
                        "count1_portable needle {needle} len {len} input {input:?}"
                    );
                    assert_eq!(
                        find1(needle, &input),
                        naive_find1(needle, &input),
                        "find1 needle {needle} len {len} input {input:?}"
                    );
                    assert_eq!(
                        find1_near(needle, &input),
                        naive_find1(needle, &input),
                        "find1_near needle {needle} len {len} input {input:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn count1_handles_every_full_block_and_tail_offset() {
        for len in 0..=STRUCTURAL_BLOCK_BYTES * 3 {
            let mut input = vec![b'x'; len];
            assert_eq!(count1(b'a', &input), 0, "length {len}");

            for at in 0..len {
                input[at] = b'a';
                assert_eq!(count1(b'a', &input), 1, "length {len}, offset {at}");
                input[at] = b'x';
            }

            input.fill(b'a');
            assert_eq!(count1(b'a', &input), len, "dense length {len}");
        }
    }

    #[test]
    fn find2_find3_and_find4_match_naive() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF0);
        let alphabet = b"abcde";
        for &len in INTERESTING_LENGTHS {
            for _ in 0..64 {
                let input: Vec<u8> = (0..len).map(|_| rng.byte_from(alphabet)).collect();
                let naive2 = input.iter().position(|&b| b == b'a' || b == b'c');
                assert_eq!(find2(b'a', b'c', &input), naive2, "find2 len {len}");
                assert_eq!(
                    find2_near(b'a', b'c', &input),
                    naive2,
                    "find2_near len {len}"
                );
                let naive3 = input
                    .iter()
                    .position(|&b| b == b'a' || b == b'c' || b == b'e');
                assert_eq!(find3(b'a', b'c', b'e', &input), naive3, "find3 len {len}");
                assert_eq!(
                    find3_near(b'a', b'c', b'e', &input),
                    naive3,
                    "find3_near len {len}"
                );
                let naive4 = input
                    .iter()
                    .position(|&b| b == b'a' || b == b'c' || b == b'd' || b == b'e');
                assert_eq!(
                    find4(b'a', b'c', b'd', b'e', &input),
                    naive4,
                    "find4 len {len}"
                );
                assert_eq!(
                    find4_near(b'a', b'c', b'd', b'e', &input),
                    naive4,
                    "find4_near len {len}"
                );
            }
        }
    }

    #[test]
    fn find_literal_matches_naive() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        let alphabet = b"ab";
        for &len in INTERESTING_LENGTHS {
            for _ in 0..64 {
                let input: Vec<u8> = (0..len).map(|_| rng.byte_from(alphabet)).collect();
                for needle in [
                    b"".as_slice(),
                    b"a",
                    b"b",
                    b"ab",
                    b"ba",
                    b"aa",
                    b"abab",
                    b"abba",
                    b"aaaa",
                ] {
                    assert_eq!(
                        find_literal(needle, &input),
                        naive_find_literal(needle, &input),
                        "needle {needle:?} len {len} input {input:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn find_literal_matches_naive_for_generated_shapes_and_offsets() {
        let mut rng = Rng(0xA076_1D64_78BD_642F);
        let alphabet = b"abcd";
        for haystack_len in 0..=160 {
            for needle_len in 0..=haystack_len.min(24) + 1 {
                let input: Vec<u8> = (0..haystack_len).map(|_| rng.byte_from(alphabet)).collect();
                let needle: Vec<u8> = (0..needle_len).map(|_| rng.byte_from(alphabet)).collect();
                assert_eq!(
                    find_literal(&needle, &input),
                    naive_find_literal(&needle, &input),
                    "needle {needle:?}, input {input:?}"
                );
            }
        }

        for needle in [
            b"ababaca".as_slice(),
            b"aaabaaaaab",
            b"abcabdabca",
            b"baaaaaaaab",
            b"cabcaabcab",
        ] {
            for match_at in [0usize, 1, 7, 31, 32, 63, 96] {
                let mut input = vec![b'x'; match_at + needle.len() + 17];
                input[match_at..match_at + needle.len()].copy_from_slice(needle);
                assert_eq!(find_literal(needle, &input), Some(match_at));
            }
        }
    }

    /// Every byte string of a given length over `alphabet`, generated in
    /// lexicographic order. Small enough lengths make this genuinely
    /// exhaustive rather than sampled, covering every short periodic shape
    /// and possible match offset.
    fn all_strings(alphabet: &[u8], len: usize) -> Vec<Vec<u8>> {
        let mut all = vec![Vec::new()];
        for _ in 0..len {
            let mut next = Vec::with_capacity(all.len() * alphabet.len());
            for prefix in &all {
                for &byte in alphabet {
                    let mut s = prefix.clone();
                    s.push(byte);
                    next.push(s);
                }
            }
            all = next;
        }
        all
    }

    /// Exhaustively checks `find_literal` against the naive oracle for every
    /// haystack/needle pair over a small alphabet, up to lengths that cover
    /// every needle shape (aperiodic, every period, every critical-position
    /// split) that can occur within a couple of the algorithm's block sizes.
    /// A 3-symbol alphabet is included too since two symbols can't produce a
    /// needle like `"abcabc"` where the maximal-suffix order actually
    /// disagrees on the critical position depending on which symbol is
    /// largest.
    #[test]
    fn find_literal_matches_naive_exhaustively_for_small_inputs() {
        for (alphabet, max_haystack_len, max_needle_len) in
            [(b"ab".as_slice(), 10, 6), (b"abc".as_slice(), 7, 4)]
        {
            let haystacks_by_len: Vec<Vec<Vec<u8>>> = (0..=max_haystack_len)
                .map(|len| all_strings(alphabet, len))
                .collect();
            let needles_by_len: Vec<Vec<Vec<u8>>> = (0..=max_needle_len)
                .map(|len| all_strings(alphabet, len))
                .collect();
            for needles in &needles_by_len {
                for needle in needles {
                    for haystacks in &haystacks_by_len {
                        for haystack in haystacks {
                            assert_eq!(
                                find_literal(needle, haystack),
                                naive_find_literal(needle, haystack),
                                "needle {needle:?} haystack {haystack:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Needles chosen to force every combination of short/long period and
    /// critical-position split against long, highly repetitive haystacks —
    /// the shapes that made the old `needle[0]`-anchored scan quadratic,
    /// because almost every candidate position matched almost the whole
    /// needle before finally failing.
    #[test]
    fn find_literal_matches_naive_for_periodic_needles_over_repetition() {
        let needles: [&[u8]; 10] = [
            b"aab",
            b"aba",
            b"abab",
            b"ababab",
            b"aabaa",
            b"aaaab",
            b"aaaaa",
            b"abcabc",
            b"abcabcabc",
            b"aaaaaaaaab", // classic naive-worst-case shape: long run, rare tail byte
        ];
        for &len in INTERESTING_LENGTHS {
            for repeat_unit in [b"a".as_slice(), b"ab", b"aab", b"abc"] {
                let haystack: Vec<u8> = repeat_unit.iter().copied().cycle().take(len).collect();
                for &needle in &needles {
                    assert_eq!(
                        find_literal(needle, &haystack),
                        naive_find_literal(needle, &haystack),
                        "needle {needle:?} unit {repeat_unit:?} len {len}"
                    );
                }
            }
        }
    }

    /// High byte values (including negatives if ever reinterpreted as `i8`)
    /// exercise the same signed/unsigned pitfalls the SIMD scan's
    /// `cast_signed` conversions already guard against, this time through
    /// the two-way engine's anchor and comparison paths.
    #[test]
    fn find_literal_matches_naive_for_non_ascii_bytes() {
        let mut rng = Rng(0x1357_9BDF_2468_ACE0);
        let alphabet: &[u8] = &[0x00, 0x7F, 0x80, 0xC3, 0xFF];
        for &len in INTERESTING_LENGTHS {
            for _ in 0..32 {
                let input: Vec<u8> = (0..len).map(|_| rng.byte_from(alphabet)).collect();
                for needle_len in 0..=5 {
                    let needle: Vec<u8> =
                        (0..needle_len).map(|_| rng.byte_from(alphabet)).collect();
                    assert_eq!(
                        find_literal(&needle, &input),
                        naive_find_literal(&needle, &input),
                        "needle {needle:?} len {len} input {input:?}"
                    );
                }
            }
        }
    }

    /// The construction that made the old anchor-on-`needle[0]` scan
    /// quadratic (a common leading byte, a rare trailing byte, run lengths
    /// far past any small-input special case) must still resolve quickly at
    /// a scale where a quadratic algorithm would not finish in any
    /// reasonable test budget. This doesn't assert a time bound (that's
    /// what `benches/literal_search.rs`'s permanent benchmark is for) — it
    /// just pins down the actual answers at a size where "doesn't finish"
    /// would itself be the failure.
    #[test]
    fn find_literal_stays_fast_on_adversarial_repetition() {
        let haystack_absent = vec![b'a'; 1_000_000];
        let needle = [b"a".repeat(49), b"z".to_vec()].concat();
        assert_eq!(find_literal(&needle, &haystack_absent), None);

        let mut haystack_present = vec![b'a'; 1_000_000];
        let match_at = haystack_present.len() - needle.len();
        haystack_present[match_at..].copy_from_slice(&needle);
        assert_eq!(find_literal(&needle, &haystack_present), Some(match_at));

        // A single early, otherwise-invisible match embedded well before the
        // tail must still be the one reported (leftmost match, not just "a
        // match exists").
        let mut haystack_early = vec![b'a'; 1_000_000];
        haystack_early[12345..12345 + needle.len()].copy_from_slice(&needle);
        haystack_early[999_000..999_000 + needle.len()].copy_from_slice(&needle);
        assert_eq!(find_literal(&needle, &haystack_early), Some(12345));
    }

    /// `rfind1` searches backwards in widening windows, so a needle further
    /// back than the initial window must still be found, and an input with no
    /// needle at all must widen all the way to the front before giving up.
    #[test]
    fn rfind1_widens_its_window_past_the_first_pass() {
        // Past 128 bytes (one widening), past 512 (two), and past 2048 (three).
        for distance in [200usize, 700, 3000] {
            let mut input = vec![b'.'; distance];
            input[0] = b'x';
            assert_eq!(
                rfind1(b'x', &input),
                Some(0),
                "a needle {distance} bytes back must be found"
            );
            assert_eq!(
                rfind1(b'y', &input),
                None,
                "widening must terminate at the front of a {distance}-byte input"
            );
        }
    }

    /// `rfind1` widens its scan window from 128 bytes by factors of four, and
    /// each widening scans only the newly exposed prefix. A byte dropped at one
    /// of those seams would be invisible to any input shorter than the initial
    /// window, so this walks a single needle across every offset of inputs that
    /// span one, two, and three widenings, plus the exact clamp to `0`.
    #[test]
    fn rfind1_finds_a_needle_at_every_offset_across_widenings() {
        for len in [129usize, 512, 513, 640, 1400, 2049] {
            for at in 0..len {
                let mut input = vec![b'.'; len];
                input[at] = b'x';
                assert_eq!(
                    rfind1(b'x', &input),
                    Some(at),
                    "needle at offset {at} of {len} must not be skipped"
                );
            }
        }
    }

    /// The same widening path, checked against the naive scan on inputs dense
    /// with matches, so the *last* occurrence is what comes back rather than
    /// merely some occurrence.
    #[test]
    fn rfind1_matches_naive_past_the_initial_window() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let alphabet = b"abc";
        for len in [129usize, 512, 513, 640, 1400, 2049] {
            for _ in 0..16 {
                let input: Vec<u8> = (0..len).map(|_| rng.byte_from(alphabet)).collect();
                for needle in *b"abz" {
                    assert_eq!(
                        rfind1(needle, &input),
                        input.iter().rposition(|&byte| byte == needle),
                        "rfind1 needle {needle} len {len}"
                    );
                }
            }
        }
    }

    #[test]
    fn rfind1_matches_naive() {
        let mut rng = Rng(0x0F0F_0F0F_1111_2222);
        let alphabet = b"abc";
        for &len in INTERESTING_LENGTHS {
            for _ in 0..64 {
                let input: Vec<u8> = (0..len).map(|_| rng.byte_from(alphabet)).collect();
                for needle in *b"abz" {
                    assert_eq!(
                        rfind1(needle, &input),
                        input.iter().rposition(|&byte| byte == needle),
                        "rfind1 needle {needle} len {len} input {input:?}"
                    );
                }
            }
        }
    }

    fn structural_offsets(input: &[u8], first: u8, second: u8, third: u8) -> Vec<usize> {
        let mut offsets = Vec::new();
        for mut block in StructuralBlocks::new(input, first, second, third) {
            while let Some(offset) = block.next_offset() {
                offsets.push(offset);
            }
        }
        offsets
    }

    #[test]
    fn finds_bytes_at_every_block_boundary() {
        for len in 0..=96 {
            let mut input = vec![b'x'; len];
            assert_eq!(find1(b'a', &input), None);
            assert_eq!(find2(b'a', b'b', &input), None);
            assert_eq!(find3_near(b'a', b'b', b'c', &input), None);

            for at in 0..len {
                input[at] = b'a';
                assert_eq!(find1(b'a', &input), Some(at));
                assert_eq!(find2(b'b', b'a', &input), Some(at));
                assert_eq!(find3_near(b'b', b'c', b'a', &input), Some(at));
                input[at] = b'x';
            }
        }
    }

    #[test]
    fn structural_blocks_match_scalar_search() {
        for len in 0usize..=128 {
            let input: Vec<_> = (0..len)
                .map(|index| {
                    u8::try_from((index * 37 + len * 11) % 251)
                        .expect("value is reduced modulo 251")
                })
                .collect();
            let expected: Vec<_> = input
                .iter()
                .enumerate()
                .filter_map(|(index, &byte)| {
                    (byte == 3 || byte == 71 || byte == 199).then_some(index)
                })
                .collect();
            assert_eq!(structural_offsets(&input, 3, 71, 199), expected);
        }
    }

    #[test]
    fn structural_blocks_find_every_position_and_tail_length() {
        for len in 0usize..=96 {
            let mut input = vec![b'x'; len];
            assert!(structural_offsets(&input, b',', b'"', b'\n').is_empty());

            for at in 0..len {
                for needle in *b",\"\n" {
                    input[at] = needle;
                    assert_eq!(
                        structural_offsets(&input, b',', b'"', b'\n'),
                        [at],
                        "length {len}, offset {at}, needle {needle}"
                    );
                    input[at] = b'x';
                }
            }
        }
    }

    #[test]
    fn structural_blocks_preserve_dense_offsets() {
        for len in 0usize..=96 {
            let input: Vec<_> = (0..len).map(|index| b",\"\n"[index % 3]).collect();
            assert_eq!(
                structural_offsets(&input, b',', b'"', b'\n'),
                (0..len).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn structural_search_resume_and_next_match() {
        let input = b"0,2,4,6,8,10,12,14,16,18,20,22,24,26,28,30,32,34,36,38,40,42,44,46,48,50";
        let mut search = StructuralBlocks::new(input, b',', b'"', b'\n');
        let mut first_block = search
            .next()
            .expect("the structural input contains commas in its first block");
        let cache = search.cache();

        let mut matches = Vec::new();
        while let Some(m) = first_block.next_match() {
            matches.push(m);
        }
        assert!(!matches.is_empty());
        assert_eq!(first_block.next_match(), None);

        // Resume within the first block using cache
        let mut resumed = StructuralBlocks::resume(input, b',', b'"', b'\n', 5, cache);
        let mut resumed_block = resumed
            .next()
            .expect("the resumed structural scan has comma matches after offset 5");
        let (first_offset, byte) = resumed_block
            .next_match()
            .expect("the resumed block contains a comma after offset 5");
        assert!(first_offset >= 5);
        assert_eq!(byte, b',');

        let mut adjacent = StructuralBlock {
            input: b",\"",
            start: 0,
            mask: 0b11,
        };
        assert_eq!(adjacent.next_match(), Some((0, b',')));
        assert_eq!(adjacent.next_match(), Some((1, b'"')));
        assert_eq!(adjacent.next_match(), None);
    }

    #[test]
    fn structural_resume_and_mask_progress_are_exact() {
        let mut input = [b'x'; STRUCTURAL_BLOCK_BYTES * 2 + 1];
        for offset in [0usize, 7, 31, 32, 47, 63, 64] {
            input[offset] = b',';
        }

        let mut search = StructuralBlocks::new(&input, b',', b'"', b'\n');
        let mut first = search.next().expect("first block has matches");
        let cache = search.cache();
        assert_eq!(core::mem::size_of::<BlockCache>(), 0);
        assert_eq!(first.matches(), 3);
        assert_eq!(
            [
                first.next_offset(),
                first.next_offset(),
                first.next_offset()
            ],
            [Some(0), Some(7), Some(31)]
        );
        assert_eq!(first.next_offset(), None);

        let mut resumed = StructuralBlocks::resume(&input, b',', b'"', b'\n', 31, cache);
        assert_eq!(resumed.next_block, 0);
        assert_eq!(resumed.skip_below, 31);
        let mut block = resumed.next().expect("resumed block retains offset 31");
        assert_eq!(block.next_match(), Some((31, b',')));
        assert_eq!(block.next_match(), None);

        let second = resumed.next().expect("second whole block has matches");
        assert_eq!(second.matches(), 3);

        let mut tail = resumed.next().expect("one-byte tail has a match");
        assert_eq!(tail.next_offset(), Some(STRUCTURAL_BLOCK_BYTES * 2));
        assert_eq!(tail.next_offset(), None);
        assert!(resumed.next().is_none());

        let mut empty = StructuralBlock {
            input: &input,
            start: 0,
            mask: 0,
        };
        assert_eq!(empty.next_offset(), None);
        assert_eq!(empty.next_match(), None);

        let no_matches = [b'x'; STRUCTURAL_BLOCK_BYTES * 2 + 1];
        assert!(
            StructuralBlocks::new(&no_matches, b',', b'"', b'\n')
                .next()
                .is_none()
        );

        let mut boundary = [b'x'; STRUCTURAL_BLOCK_BYTES];
        boundary[30] = b',';
        boundary[31] = b',';
        let mut resumed =
            StructuralBlocks::resume(&boundary, b',', b'"', b'\n', 31, BlockCache::new());
        let mut block = resumed.next().expect("offset 31 remains");
        assert_eq!(block.next_offset(), Some(31));
        assert_eq!(block.next_offset(), None);
    }
}
