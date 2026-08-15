//! Rewriting an owned record one field at a time with [`ByteRecord::set_field`].
//!
//! Each case builds a record of `width` eight-byte fields and then replaces
//! every one of them, front to back. The three cases differ only in the length
//! of the replacement relative to what it replaces: `equal` writes eight bytes
//! over eight, `longer` writes twelve, `shorter` writes four.
//!
//! | Case      |    8 |    32 |    128 | Per field, at 128 |
//! |-----------|------|-------|--------|-------------------|
//! | `equal`   |  519 |  1791 |   6879 |                54 |
//! | `longer`  | 2177 | 13735 | 134656 |              1052 |
//! | `shorter` | 1446 | 10580 | 122421 |               956 |
//!
//! The point of the table is the shape of each row, not its absolute numbers.
//! `equal` is linear in width: quadrupling the field count roughly quadruples
//! the cost, so the per-field price is flat and a full rewrite is O(n). That is
//! the equal-length short circuit doing its job — it copies the bytes in place
//! and touches exactly one endpoint, so nothing after the replaced field is
//! read or written. Without that short circuit the same three cases cost 1536,
//! 10608 and 116016, which is the quadratic shape the other two rows still
//! have; the row is 16.9 times cheaper at 128 fields and the saving grows with
//! width.
//!
//! `longer` and `shorter` are quadratic, and inherently so. From 32 fields to
//! 128 the cost rises about ninefold for a fourfold rise in width, and the
//! per-field price climbs with it. A length change has to shift every byte
//! after the field and re-encode every endpoint after it, so a front-to-back
//! pass pays that shift `width` times. A record that stores its fields
//! contiguously with end offsets cannot avoid this, which is why the
//! `set_field` documentation points callers rewriting a whole record at
//! [`ByteRecord::clear`] plus [`ByteRecord::push_field`] instead — that is
//! linear in the record's total size however the lengths change.
//!
//! So `set_field` is the right tool for touching a few fields, and for masking
//! or fixed-width substitution — where the length does not change — it is the
//! right tool at any width. It is the wrong tool for rebuilding a record.

#![expect(missing_docs, reason = "benchmark macros are private")]

use std::hint::black_box;

use coseva::ByteRecord;
use gungraun::prelude::*;

const FIELD: &[u8] = b"01234567";
const EQUAL: &[u8] = b"abcdefgh";
const LONGER: &[u8] = b"abcdefghijkl";
const SHORTER: &[u8] = b"abcd";

const WIDTH_8: usize = 8;
const WIDTH_32: usize = 32;
const WIDTH_128: usize = 128;

/// A record of `width` fields, pre-sized so no case grows a buffer while being
/// measured. The capacity covers the longest replacement so `longer` does not
/// reallocate either.
fn record(width: usize) -> ByteRecord {
    let mut record = ByteRecord::with_capacity(width, width * LONGER.len());
    for _ in 0..width {
        record.push_field(FIELD);
    }
    record
}

fn drop_it(record: ByteRecord) {
    drop(record);
}

/// Replace every field, front to back, and return the record so the teardown
/// rather than the measured body pays for dropping it.
fn rewrite(mut record: ByteRecord, replacement: &[u8]) -> ByteRecord {
    for index in 0..record.len() {
        assert!(
            record.set_field(index, replacement),
            "benchmark index out of range"
        );
    }
    assert_eq!(
        record.get(0),
        Some(replacement),
        "benchmark replacement did not land"
    );
    black_box(record)
}

#[library_benchmark]
#[bench::width_8(args = (WIDTH_8), setup = record, teardown = drop_it)]
#[bench::width_32(args = (WIDTH_32), setup = record, teardown = drop_it)]
#[bench::width_128(args = (WIDTH_128), setup = record, teardown = drop_it)]
fn equal(record: ByteRecord) -> ByteRecord {
    rewrite(record, EQUAL)
}

#[library_benchmark]
#[bench::width_8(args = (WIDTH_8), setup = record, teardown = drop_it)]
#[bench::width_32(args = (WIDTH_32), setup = record, teardown = drop_it)]
#[bench::width_128(args = (WIDTH_128), setup = record, teardown = drop_it)]
fn longer(record: ByteRecord) -> ByteRecord {
    rewrite(record, LONGER)
}

#[library_benchmark]
#[bench::width_8(args = (WIDTH_8), setup = record, teardown = drop_it)]
#[bench::width_32(args = (WIDTH_32), setup = record, teardown = drop_it)]
#[bench::width_128(args = (WIDTH_128), setup = record, teardown = drop_it)]
fn shorter(record: ByteRecord) -> ByteRecord {
    rewrite(record, SHORTER)
}

library_benchmark_group!(
    name = set_field;
    benchmarks = equal, longer, shorter
);

main!(library_benchmark_groups = set_field);
