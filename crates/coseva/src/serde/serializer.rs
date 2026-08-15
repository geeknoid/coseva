use std::fmt;
use std::marker::PhantomData;

use serde::ser;

use super::format_into_record;
use crate::byte_record::ByteRecord;
use crate::config::{Dialect, Nulls, Quoting};
use crate::emit::{ByteSink, DirectEncodeVisitor};
use crate::error::{Error, ErrorKind};
use crate::format::CsvFormat;

/// A destination the Serde serializers assemble fields into.
///
/// It has two implementors: a [`ByteRecord`], which stages the whole record for
/// callers that want one in hand, and [`DirectSink`], which frames each field
/// straight into an emitter's rollback-capable output. Sharing this trait lets
/// one serializer drive both, rather than the native and Serde paths each
/// growing their own copy.
///
/// A field is written either in one shot with [`append_field`](RecordEmit::append_field)
/// or built up with [`extend_bytes`](RecordEmit::extend_bytes) and closed with
/// [`finish_field`](RecordEmit::finish_field); an explicit NULL closes with
/// [`finish_null_field`](RecordEmit::finish_null_field). The field-closing
/// methods are fallible because framing straight into output can reject a field
/// the configured quoting cannot represent.
pub(super) trait RecordEmit {
    /// Write one complete field.
    fn append_field(&mut self, field: &[u8]) -> Result<(), Error>;

    /// Append bytes to the field currently being built.
    fn extend_bytes(&mut self, bytes: &[u8]);

    /// Close the field currently being built.
    fn finish_field(&mut self) -> Result<(), Error>;

    /// Close the current field as an explicit NULL.
    fn finish_null_field(&mut self) -> Result<(), Error>;
}

impl RecordEmit for ByteRecord {
    fn append_field(&mut self, field: &[u8]) -> Result<(), Error> {
        Self::append_field(self, field);
        Ok(())
    }

    fn extend_bytes(&mut self, bytes: &[u8]) {
        Self::extend_bytes(self, bytes);
    }

    fn finish_field(&mut self) -> Result<(), Error> {
        Self::finish_field(self);
        Ok(())
    }

    fn finish_null_field(&mut self) -> Result<(), Error> {
        Self::finish_null_field(self);
        Ok(())
    }
}

/// A [`RecordEmit`] that frames each field directly into emitter output.
///
/// Complete fields go straight through the [`DirectEncodeVisitor`]; fields built
/// incrementally accumulate in a caller-owned scratch buffer that is reused
/// across fields, so no per-field allocation happens after warmup and the whole
/// record is never copied a second time.
pub(super) struct DirectSink<'buf, 'scratch, F: CsvFormat, B: ByteSink> {
    visitor: DirectEncodeVisitor<'buf, F, B>,
    scratch: &'scratch mut Vec<u8>,
    _format: PhantomData<F>,
}

impl<'buf, 'scratch, F: CsvFormat, B: ByteSink> DirectSink<'buf, 'scratch, F, B> {
    pub(super) fn new(
        output: &'buf mut B,
        dialect: Dialect,
        quoting: Quoting,
        nulls: Nulls,
        scratch: &'scratch mut Vec<u8>,
    ) -> Self {
        scratch.clear();
        Self {
            visitor: DirectEncodeVisitor::new(output, dialect, quoting, nulls),
            scratch,
            _format: PhantomData,
        }
    }

    /// Finish the record, writing its terminator, and return the field count.
    pub(super) fn finish(self) -> usize {
        self.visitor.finish()
    }
}

impl<F: CsvFormat, B: ByteSink> RecordEmit for DirectSink<'_, '_, F, B> {
    fn append_field(&mut self, field: &[u8]) -> Result<(), Error> {
        self.visitor.write_field(field)
    }

    fn extend_bytes(&mut self, bytes: &[u8]) {
        self.scratch.extend_from_slice(bytes);
    }

    fn finish_field(&mut self) -> Result<(), Error> {
        let result = self.visitor.write_field(self.scratch.as_slice());
        self.scratch.truncate(0);
        result
    }

    fn finish_null_field(&mut self) -> Result<(), Error> {
        debug_assert!(
            self.scratch.is_empty(),
            "an explicit null field must not stage bytes"
        );
        self.visitor.write_null_field()
    }
}

/// Record-level Serde serializer. Collects fields until the record is complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Nesting {
    Reject,
    Flatten,
}

impl From<bool> for Nesting {
    fn from(allow_nested: bool) -> Self {
        if allow_nested {
            Self::Flatten
        } else {
            Self::Reject
        }
    }
}

pub(super) struct HeaderState<'a> {
    pub(super) record: &'a mut ByteRecord,
    pub(super) named: bool,
}

pub(super) struct RecordSerializer<'a, S: RecordEmit> {
    pub(super) sink: &'a mut S,
    pub(super) headers: Option<HeaderState<'a>>,
    pub(super) nesting: Nesting,
}

impl<S: RecordEmit> RecordSerializer<'_, S> {
    /// Serialize `value` into bytes and push it as the next field.
    fn push_value<T: ser::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        if self.nesting == Nesting::Flatten {
            let mut nested = RecordSerializer {
                sink: &mut *self.sink,
                headers: None,
                nesting: self.nesting,
            };
            return value.serialize(&mut nested);
        }
        let null = {
            let mut field = FieldSerializer {
                sink: &mut *self.sink,
                null: false,
                complete: false,
            };
            value.serialize(&mut field)?;
            (field.null, field.complete)
        };
        if null.0 {
            self.sink.finish_null_field()
        } else if null.1 {
            Ok(())
        } else {
            self.sink.finish_field()
        }
    }
}

impl<S: RecordEmit> ser::Serializer for &mut RecordSerializer<'_, S> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = ser::Impossible<(), Error>;
    type SerializeMap = ser::Impossible<(), Error>;
    type SerializeStruct = Self;
    type SerializeStructVariant = ser::Impossible<(), Error>;

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.sink.append_field(if v { b"true" } else { b"false" })
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i128(self, v: i128) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u128(self, v: u128) -> Result<(), Error> {
        self.sink
            .append_field(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.sink
            .append_field(ryu::Buffer::new().format(v).as_bytes())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.sink
            .append_field(ryu::Buffer::new().format(v).as_bytes())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        let mut buf = [u8::default(); 4];
        self.sink.append_field(v.encode_utf8(&mut buf).as_bytes())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.sink.append_field(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        self.sink.append_field(v)
    }

    fn serialize_none(self) -> Result<(), Error> {
        self.sink.finish_null_field()
    }

    fn serialize_some<T: ser::Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        self.sink.finish_field()
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        self.sink.finish_field()
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<(), Error> {
        self.sink.append_field(variant.as_bytes())
    }

    fn serialize_newtype_struct<T: ser::Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ser::Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "newtype enum variants are not supported in CSV serialization; \
             only unit variants are allowed",
        ))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Ok(self)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Error> {
        Ok(self)
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Ok(self)
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuple enum variants are not supported in CSV serialization; \
             only unit variants are allowed",
        ))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "maps cannot be serialized as a CSV record; \
             use a struct or tuple for typed CSV output",
        ))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        if let Some(headers) = &mut self.headers {
            headers.named = true;
        }
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "struct enum variants are not supported in CSV serialization; \
             only unit variants are allowed",
        ))
    }

    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> Result<(), Error> {
        format_into_record(&mut *self.sink, value)?;
        self.sink.finish_field()
    }
}

// RecordSerializer acts as its own SerializeSeq / SerializeTuple / SerializeStruct.

impl<S: RecordEmit> ser::SerializeSeq for &mut RecordSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ser::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push_value(value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: RecordEmit> ser::SerializeTuple for &mut RecordSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_element<T: ser::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push_value(value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: RecordEmit> ser::SerializeTupleStruct for &mut RecordSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ser::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.push_value(value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

impl<S: RecordEmit> ser::SerializeStruct for &mut RecordSerializer<'_, S> {
    type Ok = ();
    type Error = Error;

    fn serialize_field<T: ser::Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        if let Some(headers) = &mut self.headers {
            headers.record.append_field(key.as_bytes());
        }
        self.push_value(value)
    }

    fn end(self) -> Result<(), Error> {
        Ok(())
    }
}

// ── FieldSerializer ───────────────────────────────────────────────────────────

/// Field-level Serde serializer. Encodes a single field value into bytes.
struct FieldSerializer<'a, S: RecordEmit> {
    sink: &'a mut S,
    /// Set when [`Self::serialize_none`] runs, so the caller can mark the
    /// field as an explicit NULL instead of empty once serialization
    /// completes.
    null: bool,
    /// Set when a scalar was emitted directly as a complete field.
    complete: bool,
}

impl<S: RecordEmit> FieldSerializer<'_, S> {
    #[inline]
    fn finish_value(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.sink.append_field(bytes)?;
        self.complete = true;
        Ok(())
    }
}

impl<S: RecordEmit> ser::Serializer for &mut FieldSerializer<'_, S> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = ser::Impossible<(), Error>;
    type SerializeTuple = ser::Impossible<(), Error>;
    type SerializeTupleStruct = ser::Impossible<(), Error>;
    type SerializeTupleVariant = ser::Impossible<(), Error>;
    type SerializeMap = ser::Impossible<(), Error>;
    type SerializeStruct = ser::Impossible<(), Error>;
    type SerializeStructVariant = ser::Impossible<(), Error>;

    fn serialize_bool(self, v: bool) -> Result<(), Error> {
        self.finish_value(if v { b"true" } else { b"false" })
    }

    fn serialize_i8(self, v: i8) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i16(self, v: i16) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i32(self, v: i32) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i64(self, v: i64) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_i128(self, v: i128) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u8(self, v: u8) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u16(self, v: u16) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u32(self, v: u32) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u64(self, v: u64) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_u128(self, v: u128) -> Result<(), Error> {
        self.finish_value(itoa::Buffer::new().format(v).as_bytes())
    }

    fn serialize_f32(self, v: f32) -> Result<(), Error> {
        self.finish_value(ryu::Buffer::new().format(v).as_bytes())
    }

    fn serialize_f64(self, v: f64) -> Result<(), Error> {
        self.finish_value(ryu::Buffer::new().format(v).as_bytes())
    }

    fn serialize_char(self, v: char) -> Result<(), Error> {
        let mut buf = [u8::default(); 4];
        self.finish_value(v.encode_utf8(&mut buf).as_bytes())
    }

    fn serialize_str(self, v: &str) -> Result<(), Error> {
        self.finish_value(v.as_bytes())
    }

    fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
        self.finish_value(v)
    }

    fn serialize_none(self) -> Result<(), Error> {
        // Explicit NULL: record no bytes and let the caller flag the field
        // once serialization completes, rather than writing an empty field.
        self.null = true;
        Ok(())
    }

    fn serialize_some<T: ser::Serialize + ?Sized>(self, value: &T) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
        Ok(())
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<(), Error> {
        self.finish_value(variant.as_bytes())
    }

    fn serialize_newtype_struct<T: ser::Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Error> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: ser::Serialize + ?Sized>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "newtype enum variants are not supported as CSV field values",
        ))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "sequences are not supported as CSV field values; \
             use a top-level sequence for multiple fields",
        ))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuples are not supported as CSV field values",
        ))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuple structs are not supported as CSV field values",
        ))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuple enum variants are not supported as CSV field values; \
             only unit variants are allowed",
        ))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "maps are not supported as CSV field values",
        ))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "structs are not supported as CSV field values; \
             use a top-level struct for named fields",
        ))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "struct enum variants are not supported as CSV field values; \
             only unit variants are allowed",
        ))
    }

    fn collect_str<T: fmt::Display + ?Sized>(self, value: &T) -> Result<(), Error> {
        format_into_record(&mut *self.sink, value)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::serde::record;

    #[test]
    fn test_record_emit_direct_and_primitive() {
        use serde::Serialize;
        let mut rec = ByteRecord::new();
        RecordEmit::append_field(&mut rec, b"hello").unwrap();
        assert_eq!(rec.get(0), Some(&b"hello"[..]));
        let mut r = ByteRecord::new();
        record::serialize_to_record(&"hello", &mut r, false).unwrap();
        assert_eq!(r.get(0), Some(&b"hello"[..]));

        // Struct with headers and Option null field
        #[derive(serde::Serialize)]
        struct MyRec {
            a: i32,
            b: Option<String>,
            c: String,
        }
        let rec_data = MyRec {
            a: 1,
            b: None,
            c: "test".to_string(),
        };
        let mut out_rec = ByteRecord::new();
        let mut hdr_rec = ByteRecord::new();
        let mut ser = RecordSerializer {
            sink: &mut out_rec,
            headers: Some(HeaderState {
                record: &mut hdr_rec,
                named: false,
            }),
            nesting: Nesting::Flatten,
        };
        rec_data.serialize(&mut ser).unwrap();
        assert_eq!(hdr_rec.len(), 3);
        assert_eq!(out_rec.len(), 3);
        assert_eq!(out_rec.is_null(1), Some(true));

        // Nested sequence
        let nested_data = vec![vec![1, 2], vec![3, 4]];
        let mut nested_out = ByteRecord::new();
        let mut ser2 = RecordSerializer {
            sink: &mut nested_out,
            headers: None,
            nesting: Nesting::Flatten,
        };
        nested_data.serialize(&mut ser2).unwrap();
        assert_eq!(nested_out.len(), 4);

        // FieldSerializer collect_str
        let mut cs_out = ByteRecord::new();
        let mut fs = FieldSerializer {
            sink: &mut cs_out,
            null: false,
            complete: false,
        };
        serde::Serializer::collect_str(&mut fs, &format_args!("formatted {}", 42)).unwrap();
        cs_out.finish_field();
        assert_eq!(&cs_out[0], b"formatted 42");
    }

    #[test]
    fn direct_sink_clears_scratch_and_reports_the_exact_field_count() {
        let mut output = Vec::new();
        let mut scratch = b"stale".to_vec();
        let mut sink = DirectSink::<crate::format::Csv, _>::new(
            &mut output,
            Dialect::default(),
            Quoting::Necessary,
            Nulls::None,
            &mut scratch,
        );

        sink.extend_bytes(b"first");
        sink.finish_field().expect("first field");
        sink.extend_bytes(b"second");
        sink.finish_field().expect("second field");
        let fields = sink.finish();

        assert_eq!(fields, 2);
        assert_eq!(output, b"first,second\n");
        assert!(
            scratch.is_empty(),
            "finishing a field must release its bytes before the next field"
        );
    }

    #[test]
    fn nested_serialization_stays_enabled_when_requested() {
        let value = vec![vec![1_u32, 2], vec![3, 4]];
        let mut record = ByteRecord::new();
        record::serialize_to_record(&value, &mut record, true).expect("nested values flatten");
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            vec![
                b"1".as_slice(),
                b"2".as_slice(),
                b"3".as_slice(),
                b"4".as_slice()
            ]
        );
    }

    #[test]
    fn record_level_shape_errors_keep_their_diagnostics() {
        #[derive(serde::Serialize)]
        enum Newtype {
            Value(u32),
        }

        #[derive(serde::Serialize)]
        enum Tuple {
            Value(u32, u32),
        }

        #[derive(serde::Serialize)]
        enum Struct {
            Value { number: u32 },
        }

        fn error_for(value: &impl serde::Serialize) -> Error {
            let mut record = ByteRecord::new();
            record::serialize_to_record(value, &mut record, false)
                .expect_err("shape must be rejected")
        }

        let error = error_for(&Newtype::Value(1));
        assert!(error.to_string().contains(
            "newtype enum variants are not supported in CSV serialization; only unit variants are allowed"
        ));

        let error = error_for(&Tuple::Value(1, 2));
        assert!(error.to_string().contains(
            "tuple enum variants are not supported in CSV serialization; only unit variants are allowed"
        ));

        let error = error_for(&std::collections::BTreeMap::<String, u32>::new());
        assert!(error.to_string().contains(
            "maps cannot be serialized as a CSV record; use a struct or tuple for typed CSV output"
        ));

        let error = error_for(&Struct::Value { number: 1 });
        assert!(error.to_string().contains(
            "struct enum variants are not supported in CSV serialization; only unit variants are allowed"
        ));
    }

    #[test]
    fn field_struct_variant_errors_keep_their_diagnostic() {
        #[derive(serde::Serialize)]
        enum FieldValue {
            Value { number: u32 },
        }

        #[derive(serde::Serialize)]
        struct Row {
            value: FieldValue,
        }

        let mut record = ByteRecord::new();
        let error = record::serialize_to_record(
            &Row {
                value: FieldValue::Value { number: 1 },
            },
            &mut record,
            false,
        )
        .expect_err("a struct variant cannot occupy one CSV field");
        assert!(error.to_string().contains(
            "struct enum variants are not supported as CSV field values; only unit variants are allowed"
        ));
    }
}
