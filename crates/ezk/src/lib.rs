#![warn(unreachable_pub)]

pub mod error;
mod media_type;
pub mod nodes;
mod source;
pub mod sync;
mod value_range;

pub use error::{Error, ErrorKind, Result};
pub use media_type::{ConfigRange, Frame, MediaType};
pub use source::{
    BoxedSource, BoxedSourceCancelSafe, NextEventIsCancelSafe, Source, SourceEvent, SourceStream,
};
pub use value_range::{Intersect, Range, ValueRange};
