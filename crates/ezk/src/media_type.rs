use std::{fmt::Debug, sync::Arc};

pub trait MediaType: Debug + 'static {
    type ConfigRange: ConfigRange<Config = Self::Config>;
    type Config: Debug + Clone + Send + Sync + 'static;
    type FrameData: Debug + Clone + Send + Sync + 'static;
}

pub trait ConfigRange: Sized + Debug + Clone + Send + Sync + 'static {
    type Config;

    fn any() -> Self;
    fn intersect(&self, other: &Self) -> Option<Self>;
    fn contains(&self, config: &Self::Config) -> bool;
}

#[derive(Debug)]
pub struct Frame<M: MediaType> {
    /// Media specific frame data
    frame: Arc<M::FrameData>,

    pub timestamp: u64,
}

impl<M: MediaType> Clone for Frame<M> {
    fn clone(&self) -> Self {
        Self {
            frame: self.frame.clone(),
            timestamp: self.timestamp,
        }
    }
}

impl<M: MediaType> Frame<M> {
    pub fn new(data: M::FrameData, timestamp: u64) -> Self {
        Self {
            frame: Arc::new(data),
            timestamp,
        }
    }

    pub fn data(&self) -> &M::FrameData {
        &self.frame
    }

    pub fn data_mut(&mut self) -> Option<&mut M::FrameData> {
        Arc::get_mut(&mut self.frame)
    }

    pub fn make_data_mut(&mut self) -> &mut M::FrameData {
        Arc::make_mut(&mut self.frame)
    }

    pub fn is_unique(&self) -> bool {
        Arc::strong_count(&self.frame) == 1
    }

    pub fn into_data(self) -> M::FrameData {
        Arc::try_unwrap(self.frame).unwrap_or_else(|arc| (*arc).clone())
    }
}
