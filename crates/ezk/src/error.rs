use crate::ConfigRange;
use core::fmt;
use std::{convert::Infallible, error::Error as StdError};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: Box<dyn StdError + Send>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorKind {
    NegotiationFailed,
    Other,
}

impl Error {
    pub fn new<E: StdError + Send + 'static>(kind: ErrorKind, error: E) -> Self {
        Self {
            kind,
            source: Box::new(error),
        }
    }

    pub fn negotiation_failed<A: ConfigRange, B: ConfigRange>(a: Vec<A>, b: Vec<B>) -> Self {
        Self {
            kind: ErrorKind::NegotiationFailed,
            source: Box::new(IncompatibleConfig(a, b)),
        }
    }

    pub fn msg(msg: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Other,
            source: Box::<dyn StdError + Send + Sync>::from(msg.into()),
        }
    }

    pub fn other<E: StdError + Send + 'static>(error: E) -> Self {
        Self {
            kind: ErrorKind::Other,
            source: Box::new(error),
        }
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&*self.source)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            let mut prev: &dyn StdError = &*self.source;

            writeln!(f, "{}", prev)?;

            while let Some(source) = prev.source() {
                writeln!(f, "  {}", source)?;
                prev = source;
            }

            Ok(())
        } else {
            self.source.fmt(f)
        }
    }
}

#[derive(Debug)]
pub struct IncompatibleConfig<A, B>(Vec<A>, Vec<B>);

impl<A: ConfigRange, B: ConfigRange> fmt::Display for IncompatibleConfig<A, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Incompatible configs: {:?} {:?}", self.0, self.1)
    }
}

impl<A: ConfigRange, B: ConfigRange> StdError for IncompatibleConfig<A, B> {}

impl From<Infallible> for Error {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}
