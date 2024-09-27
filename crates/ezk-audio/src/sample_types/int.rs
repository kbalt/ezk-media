use super::{FloatSample, Sample};
use std::cmp::Ordering;
use std::mem::size_of;

/// A single integer sample
pub trait IntegerSample: Sample {
    /// Is the integer signed or unsigned
    const IS_SIGNED: bool;

    /// Calculates the minimum allowed sample value
    fn min_value() -> Self {
        if Self::IS_SIGNED {
            Self::from_bits(1 << Self::BITS)
        } else {
            Self::zero()
        }
    }

    /// Calculates the maximum allowed sample value
    fn max_value() -> Self {
        let rshift = 64 - Self::BITS;

        if Self::IS_SIGNED {
            Self::from_bits((!0u64) >> (rshift + 1))
        } else {
            Self::from_bits((!0u64) >> rshift)
        }
    }

    /// Returns the sample's zero value (not to be confused with [`IntegerSample::equilibrium`])
    fn zero() -> Self {
        Self::from_bits(0)
    }

    /// Returns the sample's equilibrium
    fn equilibrium_() -> Self {
        if Self::IS_SIGNED {
            Self::zero()
        } else {
            Self::from_bits(1 << (Self::BITS - 1))
        }
    }

    /// Returns a bit mask marking the used bits for this type
    fn bit_mask() -> u64 {
        bit_mask(Self::BITS)
    }

    /// Create `Self` from the given bits. Implementations must only read the number of least significant bits specified
    /// by [`Sample::BITS`].
    fn from_bits(bits: u64) -> Self;

    /// Returns the sample's bits
    fn to_bits(self) -> u64;

    /// Returns the sample's value as f64 (255 -> 255.0, not a float sample conversion)
    fn to_f64(self) -> f64;

    /// Convert the sample to a float sample ranging from -1.0 to 1.0
    fn to_float_sample_<S>(self) -> S
    where
        S: FloatSample,
    {
        let v = if Self::IS_SIGNED {
            self.to_f64() / Self::max_value().to_f64()
        } else {
            (self.to_f64() / Self::equilibrium().to_f64()) - 1.0
        };

        S::from_f64(v)
    }

    /// Convert the sample to another integer sample
    fn to_int_sample_<S>(self) -> S
    where
        S: IntegerSample,
    {
        S::from_bits(to_int_sample_inner(
            self.to_bits(),
            Self::BITS,
            S::BITS,
            Self::IS_SIGNED,
            S::IS_SIGNED,
        ))
    }
}

fn convert_signage(bits: u64, n: usize) -> u64 {
    bits ^ sign_bit(n)
}

fn to_int_sample_inner(
    bits: u64,
    src_nbits: usize,
    dst_nbits: usize,
    src_is_signed: bool,
    dst_is_signed: bool,
) -> u64 {
    let scaled_bits = match src_nbits.cmp(&dst_nbits) {
        Ordering::Less => {
            let shift = dst_nbits - src_nbits;

            bits << shift
        }
        Ordering::Equal => bits,
        Ordering::Greater => {
            let shift = src_nbits - dst_nbits;
            bits >> shift
        }
    };

    if src_is_signed == dst_is_signed {
        scaled_bits
    } else {
        convert_signage(scaled_bits, dst_nbits)
    }
}

pub(super) fn sign_bit(n: usize) -> u64 {
    1 << (n - 1)
}

pub(super) fn bit_mask(n: usize) -> u64 {
    (!0u64) >> (64 - n)
}

macro_rules! int_samples {
    ($($ty:ty),*) => {$(
        impl Sample for $ty {
            #[allow(clippy::manual_bits)] // TODO: Cannot call <$ty>::BITS without recursion happening :(
            const BITS: usize = size_of::<$ty>() * 8;

            fn equilibrium() -> Self {
                Self::equilibrium_()
            }
            fn saturating_add_(self, other: Self) -> Self {
                self.saturating_add(other)
            }
            fn saturating_sub_(self, other: Self) -> Self {
                self.saturating_sub(other)
            }
            fn saturating_mul_f64(self, amp: f64) -> Self {
                (self as f64 * amp) as Self
            }
            fn to_float_sample<S: FloatSample>(self) -> S {
                self.to_float_sample_()
            }
            fn to_int_sample<S: IntegerSample>(self) -> S {
                self.to_int_sample_()
            }

            fn from_sample<S: Sample>(s: S) -> Self {
                s.to_int_sample()
            }
        }

        impl IntegerSample for $ty {
            const IS_SIGNED: bool = <$ty>::MIN != 0;

            fn from_bits(bits: u64) -> Self {
                (bits & (Self::bit_mask())) as Self
            }

            fn to_bits(self) -> u64 {
                self as u64 & Self::bit_mask()
            }

            fn to_f64(self) -> f64 {
                self as f64
            }
        }
    )*};
}

int_samples! {
    u8, i8, u16, i16, u32, i32, u64, i64
}
