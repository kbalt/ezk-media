use self::float::FloatSample;
use self::int::IntegerSample;
use std::fmt::{Debug, Display};

mod float;
mod int;
mod x24;

pub use x24::{I24, U24};

/// A single audio sample
pub trait Sample:
    Default + Sized + PartialEq<Self> + PartialOrd + Debug + Display + Copy + 'static
{
    /// Number of bits in the sample
    const BITS: usize;

    /// Returns the sample's equilibrium
    fn equilibrium() -> Self;

    /// Add `self` and `other`, saturating at the sample's bounds
    #[must_use]
    fn saturating_add_(self, other: Self) -> Self;

    /// Subtract `other` from `self`, saturating at the sample's bounds
    #[must_use]
    fn saturating_sub_(self, other: Self) -> Self;

    /// Multiply the sample with `amp`, saturating at the sample's bounds
    #[must_use]
    fn saturating_mul_f64(self, amp: f64) -> Self;

    /// Convert Self to a [`FloatSample`]
    fn to_float_sample<S: FloatSample>(self) -> S;

    /// Convert Self to a [`IntegerSample`]
    fn to_int_sample<S: IntegerSample>(self) -> S;

    /// Create Self from the given sample, usually implemented by calling `Self::to_float_sample` or `Self::to_int_sample`
    fn from_sample<S: Sample>(s: S) -> Self;

    /// Convert Self to S
    fn to_sample<S: Sample>(self) -> S {
        S::from_sample(self)
    }
}

#[cfg(test)]
mod tests {
    use crate::sample_types::int::IntegerSample;

    use super::{Sample, I24, U24};

    #[test]
    fn convert() {
        // TODO: more tests!
        assert_eq!(127i8.to_sample::<u8>(), 255);
        assert_eq!(127i8.to_sample::<i16>(), 32512);
        assert_eq!(127i8.to_sample::<i32>(), 2_130_706_432);
        assert_eq!(255u8.to_sample::<U24>(), U24::from_num(16711680));
        assert_eq!(U24::max_value().to_sample::<u8>(), 255);
        assert_eq!(I24::max_value().to_sample::<u8>(), 255);

        // Float -> uint
        assert_eq!(1.0f32.to_sample::<u8>(), 255);
        assert_eq!(0.5f32.to_sample::<u8>(), 191);
        assert_eq!(0.0f32.to_sample::<u8>(), 127);
        assert_eq!((-0.5f32).to_sample::<u8>(), 63);
        assert_eq!((-1.0f32).to_sample::<u8>(), 0);

        // Float -> int
        assert_eq!(1.0f32.to_sample::<i8>(), 127);
        assert_eq!(0.5f32.to_sample::<i8>(), 63);
        assert_eq!(0.0f32.to_sample::<i8>(), 0);
        assert_eq!((-0.5f32).to_sample::<i8>(), -63);
        assert_eq!((-1.0f32).to_sample::<i8>(), -127);

        // uint -> Float
        assert_is_close_to(255u8.to_sample(), 1.0);
        assert_is_close_to(191u8.to_sample(), 0.5);
        assert_is_close_to(127u8.to_sample(), 0.0);
        assert_is_close_to(63u8.to_sample(), -0.5);
        assert_is_close_to(0u8.to_sample(), -1.0);

        // int -> Float
        assert_is_close_to(127i8.to_sample(), 1.0);
        assert_is_close_to(63i8.to_sample(), 0.5);
        assert_is_close_to(0i8.to_sample(), 0.0);
        assert_is_close_to((-63i8).to_sample(), -0.5);
        assert_is_close_to((-127i8).to_sample(), -1.0);
    }

    #[track_caller]
    fn assert_is_close_to(v1: f64, v2: f64) {
        let diff = (v1 - v2).abs();

        assert!(diff < 0.01, "{v1} is not close enough to {v2}");
    }
}
