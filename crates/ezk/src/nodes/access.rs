use crate::{NextEventIsCancelSafe, Result, Source, SourceEvent};
use std::pin::pin;
use tokio::sync::{mpsc, oneshot};

type AccessFn<S> = Box<dyn FnOnce(&mut S) + Send>;

pub struct Access<S> {
    source: S,

    pending_access: Option<AccessFn<S>>,
    rx: mpsc::Receiver<AccessFn<S>>,
}

#[derive(Clone)]
pub struct AccessHandle<S> {
    tx: mpsc::Sender<AccessFn<S>>,
}

impl<S> AccessHandle<S> {
    pub async fn access<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut S) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (ret_tx, ret_rx) = oneshot::channel();

        self.tx
            .send(Box::new(move |s| _ = ret_tx.send(f(s))))
            .await
            .ok()?;

        ret_rx.await.ok()
    }

    pub fn blocking_access<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut S) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (ret_tx, ret_rx) = oneshot::channel();

        self.tx
            .blocking_send(Box::new(move |s| _ = ret_tx.send(f(s))))
            .ok()?;

        ret_rx.blocking_recv().ok()
    }
}

impl<S: Source> Access<S> {
    pub fn new(source: S) -> (Self, AccessHandle<S>) {
        let (tx, rx) = mpsc::channel(1);

        let this = Self {
            source,
            pending_access: None,
            rx,
        };

        (this, AccessHandle { tx })
    }
}

impl<S: Source + NextEventIsCancelSafe> NextEventIsCancelSafe for Access<S> {}

impl<S: Source> Source for Access<S> {
    type MediaType = S::MediaType;

    async fn capabilities(
        &mut self,
    ) -> Result<Vec<<Self::MediaType as crate::MediaType>::ConfigRange>> {
        self.source.capabilities().await
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as crate::MediaType>::ConfigRange>,
    ) -> Result<<Self::MediaType as crate::MediaType>::Config> {
        self.source.negotiate_config(available).await
    }

    async fn next_event(&mut self) -> Result<SourceEvent<S::MediaType>> {
        if let Some(pending_access) = self.pending_access.take() {
            pending_access(&mut self.source);
        }

        let mut next_event = pin!(self.source.next_event());

        tokio::select! {
            event = &mut next_event => {
                return event;
            }
            Some(access_request) = self.rx.recv() => {
                self.pending_access = Some(access_request);
            }
        }

        next_event.await
    }
}
