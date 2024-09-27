use crate::{Error, MediaType, NextEventIsCancelSafe, Result, Source, SourceEvent};
use std::pin::pin;
use tokio::select;
use tokio::sync::{mpsc, oneshot};

/// Spawns it's child source in a new task.
///
/// This can have the following advantages:
/// - allows for the [`Source::next_event`] to be cancellation safe
/// - [`Source::next_event`] is called more frequently due to having it's own task
pub struct Tasked<S: Source> {
    to_task: mpsc::Sender<SourceToTaskMsg<S::MediaType>>,
    from_task: mpsc::Receiver<Result<SourceEvent<S::MediaType>>>,
}

impl<S: Source> NextEventIsCancelSafe for Tasked<S> {}

impl<S: Source> Tasked<S> {
    pub fn new(source: S) -> Self {
        let (to_task, from_source) = mpsc::channel(2);
        let (to_source, from_task) = mpsc::channel(1);

        tokio::spawn(task(source, from_source, to_source));

        Self { from_task, to_task }
    }
}

async fn task<S: Source>(
    mut source: S,
    mut from_source: mpsc::Receiver<SourceToTaskMsg<S::MediaType>>,
    to_source: mpsc::Sender<Result<SourceEvent<S::MediaType>>>,
) {
    // wait until negotiate_config is called
    loop {
        match from_source.recv().await {
            Some(SourceToTaskMsg::Capabilities { ret }) => {
                let _ = ret.send(source.capabilities().await);
            }
            Some(SourceToTaskMsg::NegotiateConfig { available, ret }) => {
                ret.send(source.negotiate_config(available).await)
                    .expect("negotiate_config was cancelled");

                // negotiate config was called, start calling next_event
                break;
            }
            None => return,
        };
    }

    let mut pending_msg: Option<SourceToTaskMsg<S::MediaType>> = None;

    'next_event: loop {
        if let Some(msg) = pending_msg.take() {
            match msg {
                SourceToTaskMsg::Capabilities { ret } => {
                    let _ = ret.send(source.capabilities().await);
                }
                SourceToTaskMsg::NegotiateConfig { available, ret } => {
                    ret.send(source.negotiate_config(available).await)
                        .expect("negotiate_config was cancelled");
                }
            }
        }

        let mut next_event = pin!(source.next_event());

        loop {
            select! {
                event = &mut next_event => {
                    if to_source.send(event).await.is_err() {
                        return;
                    }

                    continue 'next_event;
                }
                negotiate = from_source.recv() => {
                    let Some(negotiate) = negotiate else { return; };

                    // store for later until next_event has been polled to completion
                    pending_msg = Some(negotiate);
                }
            }
        }
    }
}

impl<S: Source> Source for Tasked<S> {
    type MediaType = S::MediaType;

    async fn capabilities(&mut self) -> Result<Vec<<Self::MediaType as MediaType>::ConfigRange>> {
        let (ret, recv) = oneshot::channel();

        self.to_task
            .send(SourceToTaskMsg::Capabilities { ret })
            .await
            .map_err(|_| Error::msg("Tasked's from_task dropped"))?;

        recv.await.map_err(|_| Error::msg("Tasked's ret dropped"))?
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
    ) -> Result<<Self::MediaType as MediaType>::Config> {
        let (ret, recv) = oneshot::channel();

        self.to_task
            .send(SourceToTaskMsg::NegotiateConfig { available, ret })
            .await
            .map_err(|_| Error::msg("Tasked's from_task dropped"))?;

        recv.await.map_err(|_| Error::msg("Tasked's ret dropped"))?
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        self.from_task
            .recv()
            .await
            .ok_or_else(|| Error::msg("Tasked's to_queue dropped"))?
    }
}

enum SourceToTaskMsg<M: MediaType> {
    Capabilities {
        ret: oneshot::Sender<Result<Vec<M::ConfigRange>>>,
    },
    NegotiateConfig {
        available: Vec<M::ConfigRange>,
        ret: oneshot::Sender<Result<M::Config>>,
    },
}
