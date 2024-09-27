use crate::{ConfigRange, MediaType, NextEventIsCancelSafe, Result, Source, SourceEvent};

pub struct ConfigFilter<S: Source> {
    source: S,

    filter: Vec<<S::MediaType as MediaType>::ConfigRange>,
}

impl<S: Source + NextEventIsCancelSafe> NextEventIsCancelSafe for ConfigFilter<S> {}

impl<S: Source> ConfigFilter<S> {
    pub fn new(source: S, filter: Vec<<S::MediaType as MediaType>::ConfigRange>) -> Self {
        Self { source, filter }
    }
}

impl<S: Source> Source for ConfigFilter<S> {
    type MediaType = S::MediaType;

    async fn capabilities(&mut self) -> Result<Vec<<Self::MediaType as MediaType>::ConfigRange>> {
        self.source.capabilities().await
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
    ) -> Result<<Self::MediaType as MediaType>::Config> {
        let mut new_configs = vec![];

        for config in available {
            for allowed_config in &self.filter {
                if let Some(config) = config.intersect(allowed_config) {
                    new_configs.push(config);
                }
            }
        }

        self.source.negotiate_config(new_configs).await
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        self.source.next_event().await
    }
}
