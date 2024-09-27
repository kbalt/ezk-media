use crate::{Sample, I24, U24};
use byte_slice_cast::AsByteSlice;
use ezk::ValueRange;
use std::collections::VecDeque;
use std::fmt;

impl Format {
    #[must_use]
    pub fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
    }
}

macro_rules! sample_formats {
    ($($variant:ident: $T:ident, $bits:expr;)+) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Format {
            $($variant,)+
        }

        impl Format {
            #[must_use]
            pub fn all() -> ValueRange<Self> {
                ValueRange::AnyOf(vec![
                    $(ValueRange::Value(Self::$variant),)+
                ])
            }

            #[must_use]
            pub const fn bits_per_sample(&self) -> usize {
                match self {
                    $(Self::$variant => $bits,)+
                }
            }

            #[must_use]
            pub const fn bytes_per_sample(&self) -> usize {
                self.bits_per_sample().div_ceil(8)
            }
        }

        #[derive(Clone, PartialEq)]
        pub enum Samples {
            $($variant (Vec<$T>),)+
        }

        impl Samples {
            #[must_use]
            pub const fn empty(format: Format) -> Self {
                match format {
                    $(Format::$variant => Self::$variant(Vec::new()),)+
                }
            }

            /// Create a new [`Samples`] filled with `len` amount of equilibrium samples (silence)
            #[must_use]
            pub fn equilibrium(format: Format, len: usize) -> Self {
                match format {
                    $(Format::$variant => Self::$variant(vec![<$T as Sample>::equilibrium(); len]),)+
                }
            }

            #[must_use]
            pub fn with_capacity(format: Format, capacity: usize) -> Self {
                match format {
                    $(Format::$variant => Self::$variant(Vec::with_capacity(capacity)),)+
                }
            }

            /// Returns the amount of samples in the buffer
            #[must_use]
            pub fn len(&self) -> usize {
                match self {
                    $(Self::$variant(vec) => vec.len(),)+
                }
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }

            /// Returns the sample format of the buffer
            #[must_use]
            pub fn format(&self) -> Format {
                match self {
                    $(Self::$variant(..) => Format::$variant,)+
                }
            }

            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                match self {
                    $(Self::$variant(v) => v.as_byte_slice(),)*
                }
            }

            #[must_use]
            pub fn from_bytes(format: Format, bytes: &[u8]) -> Self {
                match format {
                    $(Format::$variant => {
                        let bytes_per_sample = format.bytes_per_sample();
                        assert_eq!(bytes.len() % bytes_per_sample, 0);

                        Self::$variant(Vec::from_iter(bytes.chunks_exact(bytes_per_sample).map(|chunk| {
                            $T::from_ne_bytes(chunk.try_into().expect("from_ne_bytes must accept size_of::<Self> amount of bytes"))
                        })))
                    },)+
                }
            }
        }

        impl fmt::Debug for Samples {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant(v) => write!(f, "Samples(format = {}, len = {})", stringify!($variant), v.len()),)*
                }
            }
        }

        $(
        impl From<Vec<$T>> for Samples {
            fn from(vec: Vec<$T>) -> Self {
                Self::$variant(vec.into())
            }
        }
        )+

        pub enum SamplesQueue {
            $($variant (VecDeque<$T>),)+
        }

        impl SamplesQueue {
            #[must_use]
            pub fn empty(format: Format) -> Self {
                match format {
                    $(Format::$variant => Self::$variant(VecDeque::new()),)+
                }
            }

            #[must_use]
            pub fn len(&self) -> usize {
                match self {
                    $(Self::$variant(queue) => queue.len(),)+
                }
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }

            pub fn extend(&mut self, samples: &Samples) {
                match (self, samples) {
                    $(
                        (Self::$variant(queue), Samples::$variant(vec)) => { queue.extend(vec.iter().copied()) },
                    )+
                    _ => panic!("Tried to extend SampleQueue with different sample format"),
                }
            }

            pub fn pop_exact(&mut self, n: usize) -> Option<Samples> {
                match self {
                    $(Self::$variant(queue) => {
                        if queue.len() < n {
                            return None;
                        }

                        Some(Samples::$variant(queue.drain(..n).collect()))
                    },)+
                }
            }
        }

        impl fmt::Debug for SamplesQueue {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$variant(v) => write!(f, "SamplesQueue(format = {}, len = {})", stringify!($variant), v.len()),)*
                }
            }
        }
    }
}

sample_formats! {
    F64: f64, 64;
    F32: f32, 32;

    I32: i32, 32;
    U32: u32, 32;

    I24: I24, 24;
    U24: U24, 24;

    I16: i16, 16;
    U16: u16, 16;

    I8:  i8,  8;
    U8:  u8,  8;
}

// TODO: Wait for https://github.com/rust-lang/rust/issues/83527 and generate the macro
/// For the given format call the given code and substitute any `#S` with the correct type related to the given `format`
///
/// # Example
///
/// ```
/// # use ezk_audio::*;
/// # use std::any::TypeId;
/// fn assert_given_type_is_f32<S: 'static>() {
///     assert_eq!(
///         TypeId::of::<S>(),
///         TypeId::of::<f32>()
///     );
/// }
///
/// let format = Format::F32;
///
/// match_format!(format, assert_given_type_is_f32::<#S>());
/// ```
#[macro_export]
macro_rules! match_format {
    ($format:expr, $($input:tt)*) => {
        match $format {
            $crate::Format::F64 => { $crate::substitute!($crate::__private_macro_exports::f64 => $($input)*) }
            $crate::Format::F32 => { $crate::substitute!($crate::__private_macro_exports::f32 => $($input)*) }
            $crate::Format::I32 => { $crate::substitute!($crate::__private_macro_exports::i32 => $($input)*) }
            $crate::Format::U32 => { $crate::substitute!($crate::__private_macro_exports::u32 => $($input)*) }
            $crate::Format::I24 => { $crate::substitute!($crate::__private_macro_exports::I24 => $($input)*) }
            $crate::Format::U24 => { $crate::substitute!($crate::__private_macro_exports::U24 => $($input)*) }
            $crate::Format::I16 => { $crate::substitute!($crate::__private_macro_exports::i16 => $($input)*) }
            $crate::Format::U16 => { $crate::substitute!($crate::__private_macro_exports::u16 => $($input)*) }
            $crate::Format::I8  => { $crate::substitute!($crate::__private_macro_exports::i8  => $($input)*) }
            $crate::Format::U8  => { $crate::substitute!($crate::__private_macro_exports::u8  => $($input)*) }
        }
    };
}

/// Match one or more [`Samples`] and call a given function with the associated sample type
///
/// ```ignore
/// let samples = Samples::empty(Format::F32);
///
/// match_samples!((&samples) => (s) => { println!("{}", s.len()) });
///
/// // This basically expands to the following
/// match (&samples,) {
///     (Samples::F64(s),) => { println!("{}", s.len()) }
///     (Samples::F32(s),) => { println!("{}", s.len()) }
///     ...
/// }
///
/// // When matching multiple samples
///  let samples1 = Samples::empty(Format::F32);
///  let samples2 = Samples::empty(Format::F32);
///
/// match_samples!((&samples1, &samples2) => (s1, s2) => { println!("{} {}", s1.len(), s2.len()) });
///
/// // This expands to the following
/// match (&samples1, &samples2,) {
///     (Samples::F64(s1), Samples::F64(s2),) => { println!("{} {}", s1.len(), s2.len()) }
///     (Samples::F32(s1), Samples::F64(s2),) => { println!("{} {}", s1.len(), s2.len()) }
///     ...
///     _ => panic!("Tried to match multiple samples with different sample types"),
/// }
/// ```
///
/// # Example
///
/// ```
/// # use ezk_audio::*;
/// # use std::any::TypeId;
/// fn assert_given_type_is_f32<S: 'static>(samples: &[S]) {
///     assert_eq!(samples.len(), 100);
///     assert_eq!(
///         TypeId::of::<S>(),
///         TypeId::of::<f32>()
///     );
/// }
///
/// let samples = Samples::equilibrium(Format::F32, 100);
///
/// // matching a single samples
/// match_samples!((&samples) => (s) => assert_given_type_is_f32::<#S>(s));
///
/// // matching a multiple samples
/// match_samples!((&samples, &samples) => (s, _s) => assert_given_type_is_f32::<#S>(s));
/// ```
#[macro_export]
macro_rules! match_samples {
    (($($samples:expr),*) => ($($pat:pat),*) => $($input:tt)*) => {
        match ($($samples,)*) {
            ($($crate::Samples::F64($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::f64 => $($input)*) }
            ($($crate::Samples::F32($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::f32 => $($input)*) }
            ($($crate::Samples::I32($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::i32 => $($input)*) }
            ($($crate::Samples::U32($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::u32 => $($input)*) }
            ($($crate::Samples::I24($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::I24 => $($input)*) }
            ($($crate::Samples::U24($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::U24 => $($input)*) }
            ($($crate::Samples::I16($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::i16 => $($input)*) }
            ($($crate::Samples::U16($pat),)*) => { $crate::substitute!($crate::__private_macro_exports::u16 => $($input)*) }
            ($($crate::Samples::I8($pat),)*)  => { $crate::substitute!($crate::__private_macro_exports::i8  => $($input)*) }
            ($($crate::Samples::U8($pat),)*)  => { $crate::substitute!($crate::__private_macro_exports::u8  => $($input)*) }

            #[allow(unreachable_patterns)] _ => panic!("Tried to match multiple samples with different sample types")
        }
    };
}

/// Substitute #S with the given $i1 in any type
///
/// # Example
///
/// ```
/// # use ezk_audio::*;
/// fn some_func<T>() {}
/// substitute!(i64 => some_func::<#S>()); // expands to some_func::<i64>();
/// ```
#[macro_export]
#[doc(hidden)]
macro_rules! substitute {
    ($i1:ty => $($input:tt)*) => {
        $crate::substitute!(@inner $i1 => [] [$($input)*])
    };
    (@inner
        $i1:ty =>
        [$($processed:tt)*]
        [#S $($input:tt)*]
    ) => {
        $crate::substitute!(@inner $i1 =>
            [$($processed)* $i1]
            [$($input)*]
        )
    };
    (@inner
        $i1:ty =>
        [$($processed:tt)*]
        [{ $($block_input:tt)* } $($input:tt)*]
    ) => {
        $crate::substitute!(@inner $i1 =>
            [$($processed)* { $crate::substitute! { $i1 => $($block_input)* } }]
            [$($input)*]
        )
    };
    (@inner
        $i1:ty =>
        [$($processed:tt)*]
        [$token:tt $($input:tt)*]
    ) => {
        $crate::substitute!(@inner $i1 =>
            [$($processed)* $token]
            [$($input)*]
        )
    };
    (@inner
        $i1:ty =>
        [$($processed:tt)*]
        []
    ) => {
        $($processed)*
    };
}
