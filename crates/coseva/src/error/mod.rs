//! The crate's unified error type.
//!
//! Every fallible operation reports [`Error`], with an [`ErrorKind`] and a
//! [`Location`] carrying its byte offset, line, record, and field.

mod error_type;
mod kind;
mod location;

#[doc(inline)]
pub use error_type::{Error, Result};
#[doc(inline)]
pub use kind::ErrorKind;
#[doc(inline)]
pub use location::Location;
