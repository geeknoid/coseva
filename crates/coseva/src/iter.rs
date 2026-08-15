//! Iterators over independently owned records.
//!
//! Every iterator here yields owned values, because [`Iterator`] cannot lend
//! an item that borrows the parser it came from. Reach for [`crate::Line`]
//! instead when the record can be read in place, or when a reusable buffer
//! should be filled rather than a fresh one allocated per record.
//!
//! Each iterator comes in a plain and a `matching_` form. Filtering is a
//! field of the same type rather than a type of its own, so pushing a
//! predicate down into the scan never doubles the surface.

use core::borrow::BorrowMut;
use core::marker::PhantomData;
use core::mem;

use crate::byte_record::ByteRecord;
use crate::encoding::{CsvDecodeOwned, DecodeNew};
use crate::engine::TypedMapping;
use crate::error::Error;
use crate::filter::Predicate;
use crate::line::LineSource;
use crate::text_record::TextRecord;

/// The state every record iterator keeps.
///
/// Holding the cursor in one place keeps the four iterators to the parts that
/// actually differ: what they do with a line once the scan has found one.
///
/// `H` is how the parser is held. A run started from a parser method borrows
/// it, and one started from a whole-document entry point owns it, so that the
/// iterator can outlive the expression that built the parser. Both are
/// [`BorrowMut<P>`], so the record kinds below are written once.
#[derive(Debug)]
struct Cursor<'parser, P: ?Sized, H> {
    /// The parser the records are pulled from, held either way.
    parser: H,
    /// The pushdown filter, when the iterator was built by a `matching_` call.
    predicate: Option<&'parser Predicate>,
    /// Whether the run has ended, so that a reported error is not retried.
    done: bool,
    /// The mapping of a target type's fields onto the header record, resolved
    /// on first use and reused for the rest of the run. Only a typed run needs
    /// one.
    mapping: Option<TypedMapping>,
    /// Ties the cursor to the parser type it borrows through.
    marker: PhantomData<fn() -> *mut P>,
}

impl<'parser, P: ?Sized, H: BorrowMut<P>> Cursor<'parser, P, H> {
    /// Start a run over `parser`, optionally filtered by `predicate`.
    fn new(parser: H, predicate: Option<&'parser Predicate>) -> Self {
        Self {
            parser,
            predicate,
            done: false,
            mapping: None,
            marker: PhantomData,
        }
    }

    fn finish(&mut self) {
        let _ = mem::replace(&mut self.done, true);
    }

    /// End the run and hand back the reason.
    fn end(&mut self, error: Error) -> Error {
        self.finish();
        error
    }
}

impl<P: LineSource + ?Sized, H: BorrowMut<P>> Cursor<'_, P, H> {
    /// Move to the next record, ending the run at input end or on error.
    ///
    /// Returns `None` once the run is over, so a caller can simply propagate
    /// it, and `Some(Err(_))` exactly once for a failure.
    fn step(&mut self) -> Option<Result<(), Error>> {
        if self.done {
            return None;
        }
        match self.parser.borrow_mut().advance_line(self.predicate) {
            Ok(true) => Some(Ok(())),
            Ok(false) => {
                self.finish();
                None
            }
            Err(error) => Some(Err(self.end(error))),
        }
    }
}

/// Generate an iterator over owned records of one kind.
///
/// The four iterators share their whole cursor protocol and differ only in
/// the view they take of a located line, so only that view is written out.
macro_rules! record_iterator {
    (
        $(#[$meta:meta])*
        $name:ident<$item:ty> $(where T: $bound:path)?,
        |$cursor:ident| $body:block
    ) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub struct $name<'parser, P: ?Sized, T = (), H = &'parser mut P> {
            cursor: Cursor<'parser, P, H>,
            marker: PhantomData<fn() -> T>,
        }

        impl<'parser, P: ?Sized, T, H: BorrowMut<P>> $name<'parser, P, T, H> {
            /// Start a run over `parser`, optionally filtered by `predicate`.
            pub(crate) fn new(
                parser: H,
                predicate: Option<&'parser Predicate>,
            ) -> Self {
                Self {
                    cursor: Cursor::new(parser, predicate),
                    marker: PhantomData,
                }
            }
        }

        impl<P: LineSource + ?Sized, T, H: BorrowMut<P>> Iterator for $name<'_, P, T, H>
        $(where T: $bound)?
        {
            type Item = Result<$item, Error>;

            fn next(&mut self) -> Option<Self::Item> {
                let $cursor = self;
                $body
            }
        }

        impl<P: LineSource + ?Sized, T, H: BorrowMut<P>> core::iter::FusedIterator
            for $name<'_, P, T, H>
        $(where T: $bound)?
        {
        }
    };
}

record_iterator! {
    /// Iterator over owned byte records.
    ///
    /// Each item is freshly allocated. Use [`crate::Line::read_byte_record_into`]
    /// to refill one record instead.
    /// For a worked example, see [`crate::SliceParser::byte_records`].
    ByteRecords<ByteRecord>,
    |this| {
        if let Err(error) = this.cursor.step()? {
            return Some(Err(error));
        }
        let mut output = ByteRecord::new();
        match this.cursor.parser.borrow_mut().line_view().read_byte_record_into(&mut output) {
            Ok(()) => Some(Ok(output)),
            Err(error) => Some(Err(this.cursor.end(error))),
        }
    }
}

record_iterator! {
    /// Iterator over owned records of validated UTF-8.
    ///
    /// Each item is freshly allocated. Use [`crate::Line::read_text_record_into`]
    /// to refill one record instead.
    /// For a worked example, see [`crate::SliceParser::text_records`].
    TextRecords<TextRecord>,
    |this| {
        if let Err(error) = this.cursor.step()? {
            return Some(Err(error));
        }
        let mut output = TextRecord::new();
        match this.cursor.parser.borrow_mut().line_view().read_text_record_into(&mut output) {
            Ok(()) => Some(Ok(output)),
            Err(error) => Some(Err(this.cursor.end(error))),
        }
    }
}

record_iterator! {
    /// Iterator over independently owned typed records.
    ///
    /// The mapping from the target type's fields onto the headers is resolved
    /// once for the whole run, and only the fields the type names are
    /// materialized.
    /// For a worked example, see [`crate::encoding::CsvDecode`].
    DecodedRecords<T> where T: CsvDecodeOwned,
    |this| {
        if this.cursor.mapping.is_none() {
            match this.cursor
                .parser
                .borrow_mut()
                .resolve_typed_mapping(T::field_names(), T::field_aliases()) {
                Ok(mapping) => this.cursor.mapping = Some(mapping),
                Err(error) => return Some(Err(this.cursor.end(error))),
            }
        }
        if let Err(error) = this.cursor.step()? {
            return Some(Err(error));
        }
        let cursor = &mut this.cursor;
        let mapping = cursor
            .mapping
            .as_ref()
            .expect("the mapping is resolved above");
        match cursor.parser.borrow_mut().decode_through(mapping, DecodeNew::<T>::new()) {
            Ok(decoded) => Some(Ok(decoded)),
            Err(error) => Some(Err(this.cursor.end(error))),
        }
    }
}

#[cfg(feature = "serde")]
record_iterator! {
    /// Iterator over independently owned Serde-deserialized records.
    ///
    /// For a worked example, see [`crate::deserialize_from_slice`].
    DeserializedRecords<T> where T: ::serde::de::DeserializeOwned,
    |this| {
        if let Err(error) = this.cursor.step()? {
            return Some(Err(error));
        }
        // An owned value borrows nothing from the window, so the record view
        // built for it ends with this call.
        match this.cursor.parser.borrow_mut().line_view().deserialized::<T>() {
            Ok(value) => Some(Ok(value)),
            Err(error) => Some(Err(this.cursor.end(error))),
        }
    }
}

/// Generate the owned-record iterator surface over a parser type.
///
/// Each kind comes in a plain and a `matching_` form, which differ only in
/// the predicate they hand the scan.
macro_rules! record_iterators {
    ([$($generics:tt)*], $parser:ty) => {
        impl<$($generics)*> $parser {
            /// Iterate over owned byte records.
            ///
            /// Each item is freshly allocated, so prefer
            /// [`Self::next_line`] with
            /// [`Line::read_byte_record_into`](crate::Line::read_byte_record_into)
            /// when one record buffer can be reused across the loop.
            #[inline]
            pub fn byte_records(&mut self) -> ByteRecords<'_, Self> {
                ByteRecords::new(self, None)
            }

            /// Iterate over the owned byte records satisfying `predicate`.
            ///
            /// A record that does not match is skipped without being split
            /// into fields, so most of the document costs almost nothing when
            /// matches are rare. See [`Predicate`].
            #[inline]
            pub fn matching_byte_records<'scan>(
                &'scan mut self,
                predicate: &'scan Predicate,
            ) -> ByteRecords<'scan, Self> {
                ByteRecords::new(self, Some(predicate))
            }

            /// Iterate over owned records of validated UTF-8.
            ///
            /// Each item is freshly allocated, so prefer
            /// [`Self::next_line`] with
            /// [`Line::read_text_record_into`](crate::Line::read_text_record_into)
            /// when one record buffer can be reused across the loop.
            #[inline]
            pub fn text_records(&mut self) -> TextRecords<'_, Self> {
                TextRecords::new(self, None)
            }

            /// Iterate over the owned records of validated UTF-8 satisfying
            /// `predicate`.
            ///
            /// A record that does not match is skipped without being split
            /// into fields, so most of the document costs almost nothing when
            /// matches are rare. See [`Predicate`].
            #[inline]
            pub fn matching_text_records<'scan>(
                &'scan mut self,
                predicate: &'scan Predicate,
            ) -> TextRecords<'scan, Self> {
                TextRecords::new(self, Some(predicate))
            }

            /// Iterate over independently owned typed records.
            ///
            /// Only the fields the target type names are materialized, so a
            /// projected type never pays for the columns it ignores.
            ///
            /// ```
            /// use coseva::config::ParseOptions;
            /// use coseva::format::Csv;
            /// use coseva::SliceParser;
            /// # #[cfg(feature = "derive")] {
            /// use coseva::encoding::CsvDecode;
            ///
            /// #[derive(CsvDecode)]
            /// struct City {
            ///     name: String,
            ///     population: u64,
            /// }
            ///
            /// let mut parser = SliceParser::<Csv>::new(
            ///     b"name,population\nBoston,650706\nDenver,715522\n",
            ///     ParseOptions::new(),
            /// )?;
            /// let cities: Vec<City> = parser.decoded_records().collect::<Result<_, _>>()?;
            /// assert_eq!(cities.len(), 2);
            /// assert_eq!(cities[1].name, "Denver");
            /// # }
            /// # Ok::<(), coseva::Error>(())
            /// ```
            #[inline]
            pub fn decoded_records<T>(&mut self) -> DecodedRecords<'_, Self, T>
            where
                T: CsvDecodeOwned,
            {
                DecodedRecords::new(self, None)
            }

            /// Iterate over the independently owned typed records satisfying
            /// `predicate`.
            ///
            /// A record that does not match is skipped without being split
            /// into fields, so it is never decoded. See [`Predicate`].
            #[inline]
            pub fn matching_decoded_records<'scan, T>(
                &'scan mut self,
                predicate: &'scan Predicate,
            ) -> DecodedRecords<'scan, Self, T>
            where
                T: CsvDecodeOwned,
            {
                DecodedRecords::new(self, Some(predicate))
            }

            /// Iterate over independently owned Serde-deserialized records.
            ///
            /// Headers are discovered lazily on the first call to `next()`.
            ///
            /// ```
            /// use coseva::config::ParseOptions;
            /// use coseva::format::Csv;
            /// use coseva::SliceParser;
            /// use serde::Deserialize;
            ///
            /// #[derive(Deserialize)]
            /// struct City {
            ///     name: String,
            ///     population: u64,
            /// }
            ///
            /// let mut parser = SliceParser::<Csv>::new(
            ///     b"name,population\nBoston,650706\nDenver,715522\n",
            ///     ParseOptions::new(),
            /// )?;
            /// let cities: Vec<City> = parser.deserialized_records().collect::<Result<_, _>>()?;
            /// assert_eq!(cities.len(), 2);
            /// assert_eq!(cities[1].name, "Denver");
            /// # Ok::<(), coseva::Error>(())
            /// ```
            #[cfg(feature = "serde")]
            #[inline]
            pub fn deserialized_records<T>(&mut self) -> DeserializedRecords<'_, Self, T>
            where
                T: ::serde::de::DeserializeOwned,
            {
                DeserializedRecords::new(self, None)
            }

            /// Iterate over the independently owned Serde-deserialized records
            /// satisfying `predicate`.
            ///
            /// A record that does not match is skipped without being split
            /// into fields, so it is never deserialized. See [`Predicate`].
            #[cfg(feature = "serde")]
            #[inline]
            pub fn matching_deserialized_records<'scan, T>(
                &'scan mut self,
                predicate: &'scan Predicate,
            ) -> DeserializedRecords<'scan, Self, T>
            where
                T: ::serde::de::DeserializeOwned,
            {
                DeserializedRecords::new(self, Some(predicate))
            }
        }
    };
}

record_iterators!(
    ['input, F: crate::format::CsvFormat],
    crate::slice_parser::SliceParser<'input, F>
);
#[cfg(feature = "std")]
record_iterators!(
    [R: ::std::io::Read, F: crate::format::CsvFormat],
    crate::io_parser::IoParser<R, F>
);

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::SliceParser;
    use crate::config::{Headers, ParseOptions};
    use crate::error::ErrorKind;
    use crate::format::Csv;

    #[test]
    fn ending_a_cursor_marks_it_done() {
        let mut parser = ();
        let mut cursor: Cursor<'_, (), &mut ()> = Cursor::new(&mut parser, None);
        let error = Error::detailed(ErrorKind::Decode, "failed");
        assert_eq!(cursor.end(error).kind(), ErrorKind::Decode);
        assert!(cursor.done);
    }

    #[test]
    fn reaching_input_end_marks_the_cursor_done() {
        let mut parser = SliceParser::<Csv>::new(b"", ParseOptions::new().headers(Headers::None))
            .expect("valid parser");
        let mut cursor: Cursor<'_, SliceParser<'_, Csv>, _> = Cursor::new(&mut parser, None);
        assert!(cursor.step().is_none());
        assert!(cursor.done);
        assert!(cursor.step().is_none());
    }
}
