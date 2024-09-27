use super::{FloatSample, IntegerSample, Sample};
use byte_slice_cast::{ToByteSlice, ToMutByteSlice};
use core::fmt;

macro_rules! make_24_bit_integer {
    ($name:ident; $num:ty; is_signed: $is_signed:expr) => {
        #[derive(Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
        #[repr(transparent)]
        pub struct $name([u8; 3]);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.num(), f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.num(), f)
            }
        }

        unsafe impl ToByteSlice for $name {
            fn to_byte_slice<T: AsRef<[Self]> + ?Sized>(slice: &T) -> &[u8] {
                let slice = slice.as_ref();
                let len = slice.len() * std::mem::size_of::<$name>();
                unsafe { std::slice::from_raw_parts(slice.as_ptr().cast(), len) }
            }
        }

        unsafe impl ToMutByteSlice for $name {
            fn to_mut_byte_slice<T: AsMut<[Self]> + ?Sized>(slice: &mut T) -> &mut [u8] {
                let slice = slice.as_mut();
                let len = slice.len() * std::mem::size_of::<$name>();
                unsafe { std::slice::from_raw_parts_mut(slice.as_mut_ptr().cast(), len) }
            }
        }

        impl $name {
            #[cfg(target_endian = "big")]
            pub fn num(self) -> $num {
                let [b1, b2, b3] = self.0;
                <$num>::from_be_bytes([0, b1, b2, b3])
            }

            #[cfg(target_endian = "big")]
            pub fn from_num(num: $num) -> Self {
                let [_b0, b1, b2, b3] = num.to_be_bytes();
                Self([b1, b2, b3])
            }

            #[cfg(target_endian = "big")]
            pub fn from_be_bytes(bytes: [u8; 3]) -> Self {
                Self(bytes)
            }

            #[cfg(target_endian = "big")]
            pub fn from_le_bytes(bytes: [u8; 3]) -> Self {
                let [b0, b1, b2] = bytes;
                Self([b2, b1, b0])
            }

            #[cfg(target_endian = "little")]
            pub fn num(self) -> $num {
                let [b1, b2, b3] = self.0;
                <$num>::from_le_bytes([b1, b2, b3, 0])
            }

            #[cfg(target_endian = "little")]
            pub fn from_num(num: $num) -> Self {
                let [b0, b1, b2, _b3] = num.to_le_bytes();
                Self([b0, b1, b2])
            }

            #[cfg(target_endian = "little")]
            pub fn from_be_bytes(bytes: [u8; 3]) -> Self {
                let [b0, b1, b2] = bytes;
                Self([b2, b1, b0])
            }

            #[cfg(target_endian = "little")]
            pub fn from_le_bytes(bytes: [u8; 3]) -> Self {
                Self(bytes)
            }

            pub fn from_ne_bytes(bytes: [u8; 3]) -> Self {
                if cfg!(target_endian = "little") {
                    Self::from_le_bytes(bytes)
                } else {
                    Self::from_be_bytes(bytes)
                }
            }

            pub fn to_ne_bytes(self) -> [u8; 3] {
                self.0
            }
        }

        impl IntegerSample for $name {
            const IS_SIGNED: bool = $is_signed;

            fn from_bits(bits: u64) -> Self {
                Self::from_num(<$num>::from_bits(bits))
            }

            fn to_bits(self) -> u64 {
                self.num() as u64
            }

            fn to_f64(self) -> f64 {
                self.num() as f64
            }
        }

        impl Sample for $name {
            const BITS: usize = 24;

            fn equilibrium() -> Self {
                Self::equilibrium_()
            }

            fn saturating_add_(self, other: Self) -> Self {
                Self::from_num(
                    self.num()
                        .saturating_add(other.num())
                        .min(Self::max_value().num()),
                )
            }

            fn saturating_sub_(self, other: Self) -> Self {
                Self::from_num(
                    self.num()
                        .saturating_sub(other.num())
                        .min(Self::max_value().num()),
                )
            }

            fn saturating_mul_f64(self, amp: f64) -> Self {
                let num = self.num() as f64;
                let num = num * amp;
                let num = num as $num;
                let num = num.clamp(Self::min_value().num(), Self::max_value().num());
                Self::from_num(num)
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

        impl From<u8> for $name {
            fn from(n: u8) -> Self {
                Self::from_num(n as _)
            }
        }
        impl From<i8> for $name {
            fn from(n: i8) -> Self {
                Self::from_num(n as _)
            }
        }
        impl From<u16> for $name {
            fn from(n: u16) -> Self {
                Self::from_num(n as _)
            }
        }

        impl From<i16> for $name {
            fn from(n: i16) -> Self {
                Self::from_num(n as _)
            }
        }
    };
}

make_24_bit_integer!(I24; i32; is_signed: true);
make_24_bit_integer!(U24; u32; is_signed: false);
