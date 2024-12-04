use std::hash::{BuildHasher, Hasher};

#[derive(Default)]
pub(crate) struct U32Hasher {
    v: u64,
}

impl BuildHasher for U32Hasher {
    type Hasher = Self;

    fn build_hasher(&self) -> Self::Hasher {
        Self::default()
    }
}

impl Hasher for U32Hasher {
    fn finish(&self) -> u64 {
        self.v
    }

    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }

    fn write_u32(&mut self, i: u32) {
        self.v = i.into();
    }
}
