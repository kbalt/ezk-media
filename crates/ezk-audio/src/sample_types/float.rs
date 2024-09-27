use super::{IntegerSample, Sample};
use std::mem::size_of;

/// A single float sample
pub trait FloatSample: Sample {
    fn to_f64(self) -> f64;

    fn from_f64(v: f64) -> Self;

    fn to_float_sample_<S>(self) -> S
    where
        S: FloatSample,
    {
        S::from_f64(self.to_f64())
    }

    fn to_int_sample_<S>(self) -> S
    where
        S: IntegerSample,
    {
        if S::IS_SIGNED {
            S::max_value().saturating_mul_f64(self.to_f64())
        } else {
            S::max_value().saturating_mul_f64((self.to_f64() + 1.0) / 2.0)
        }
    }
}

macro_rules! float_samples {
    ($($ty:ty),*) => {$(
        impl Sample for $ty {
            #[allow(clippy::manual_bits)] // TODO: Cannot call <$ty>::BITS without recursion happening :(
            const BITS: usize = size_of::<$ty>() * 8;

            fn equilibrium() -> Self {
                0.0
            }
            fn saturating_add_(self, other: Self) -> Self {
                (self + other).clamp(-1.0, 1.0)
            }
            fn saturating_sub_(self, other: Self) -> Self {
                (self - other).clamp(-1.0, 1.0)
            }

            fn to_float_sample<S: FloatSample>(self) -> S {
                self.to_float_sample_()
            }
            fn to_int_sample<S: IntegerSample>(self) -> S {
                self.to_int_sample_()
            }

            fn from_sample<S: Sample>(s: S) -> Self {
                s.to_float_sample()
            }

            fn saturating_mul_f64(self, amp: f64) -> Self {
                (self as f64 * amp) as Self
            }
        }

        impl FloatSample for $ty {
            fn to_f64(self) -> f64 {
                f64::from(self)
            }

            fn from_f64(v: f64) -> Self {
                v as Self
            }
        }
    )*};
}

float_samples! { f32, f64 }
