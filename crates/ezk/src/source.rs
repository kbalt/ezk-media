use crate::{Frame, MediaType, Result};
use downcast_rs::Downcast;
use ouroboros::self_referencing;
use reusable_box::{ReusableBox, ReusedBoxFuture};
use std::{
    future::Future,
    mem::take,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_stream::Stream;

/// Types that implement [`Source`] and are allowed to prematurely drop the [`next_event`](Source::next_event) future.
///
/// Code that uses something like tokio's [`select!`] macro may cancel the call to `next_event`.
/// This cancellation is only "safe" if the `Source` is marked by this trait.
///
/// When using [`BoxedSource`] try using [`BoxedSourceCancelSafe`] instead if this trait is required. See [`Source::boxed_cancel_safe`].
pub trait NextEventIsCancelSafe {}

#[derive(Debug)]
pub enum SourceEvent<M: MediaType> {
    /// Source produced a frame
    Frame(Frame<M>),

    /// This source will provide no more data
    // TODO: should this be a permanent, or should a source be able to restart?
    EndOfData,

    /// After this event is received the source must be renegotiated using [`Source::negotiate_config`]. This may
    /// occur when upstream sources (or their dependencies) change.
    RenegotiationNeeded,
}

pub trait Source: Send + Sized + 'static {
    type MediaType: MediaType;

    fn capabilities(
        &mut self,
    ) -> impl Future<Output = Result<Vec<<Self::MediaType as MediaType>::ConfigRange>>> + Send;

    /// Provide a list of available configurations ranges. Upstream sources then filter out invalid/incompatible
    /// configs until the "root" source then decides on the streaming config, which is then returned down the source
    /// stack. This must always be called before trying to call [`Source::next_event`].
    ///
    /// # Cancel safety
    ///
    /// This method should __never__ be considered cancel safe, as it may leave some sources in the stack configured and others not.
    fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
    ) -> impl Future<Output = Result<<Self::MediaType as MediaType>::Config>> + Send;

    /// Fetch the next event from the source. This method should be called as much as possible to
    /// allow source to drive their internal logic without having to rely on extra tasks.
    ///
    /// Should return [`SourceEvent::RenegotiationNeeded`] when called before [`negotiate_config`](Source::negotiate_config).
    ///
    /// # Cancel safety
    ///
    /// The cancel safety of this method is marked using the [`NextEventIsCancelSafe`] trait
    fn next_event(&mut self) -> impl Future<Output = Result<SourceEvent<Self::MediaType>>> + Send;

    /// Erase the type of this source
    fn boxed(self) -> BoxedSource<Self::MediaType> {
        BoxedSource {
            source: Box::new(self),
            reusable_box: ReusableBox::new(),
        }
    }

    /// Erase the type of this source, keeping the information that the `next_event` future is cancel safe
    fn boxed_cancel_safe(self) -> BoxedSourceCancelSafe<Self::MediaType>
    where
        Self: NextEventIsCancelSafe,
    {
        BoxedSourceCancelSafe::new(self)
    }
}

/// Type erased source
///
/// Used when generics are not possible since one wants to store different source or to avoid deep generic nestings
/// which inevitably leads to long compile times.
pub struct BoxedSource<M: MediaType> {
    source: Box<dyn DynSource<MediaType = M>>,

    // to avoid frequent reallocations of futures
    // every future is allocated using this reusable box
    reusable_box: ReusableBox,
}

impl<M: MediaType> BoxedSource<M> {
    #[inline]
    pub fn new(source: impl Source<MediaType = M>) -> Self {
        source.boxed()
    }

    #[inline]
    pub fn downcast<T: Source<MediaType = M>>(self) -> Result<Box<T>, Self> {
        self.source.downcast::<T>().map_err(|source| Self {
            source,
            reusable_box: self.reusable_box,
        })
    }

    #[inline]
    pub fn downcast_ref<T: Source<MediaType = M>>(&self) -> Option<&T> {
        self.source.downcast_ref::<T>()
    }

    #[inline]
    pub fn downcast_mut<T: Source<MediaType = M>>(&mut self) -> Option<&mut T> {
        self.source.downcast_mut::<T>()
    }
}

impl<M: MediaType> Source for BoxedSource<M> {
    type MediaType = M;

    async fn capabilities(&mut self) -> Result<Vec<<Self::MediaType as MediaType>::ConfigRange>> {
        self.source.capabilities(&mut self.reusable_box).await
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
    ) -> Result<<Self::MediaType as MediaType>::Config> {
        self.source
            .negotiate_config(available, &mut self.reusable_box)
            .await
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        self.source.next_event(&mut self.reusable_box).await
    }

    fn boxed(self) -> BoxedSource<Self::MediaType> {
        self
    }

    fn boxed_cancel_safe(self) -> BoxedSourceCancelSafe<Self::MediaType>
    where
        Self: NextEventIsCancelSafe,
    {
        BoxedSourceCancelSafe(self)
    }
}

// Type erased object safe source, only used in BoxedSource
trait DynSource: Downcast + Send + 'static {
    type MediaType: MediaType;

    fn capabilities<'a>(
        &'a mut self,
        bx: &'a mut ReusableBox,
    ) -> ReusedBoxFuture<'a, Result<Vec<<Self::MediaType as MediaType>::ConfigRange>>>;

    fn negotiate_config<'a>(
        &'a mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
        bx: &'a mut ReusableBox,
    ) -> ReusedBoxFuture<'a, Result<<Self::MediaType as MediaType>::Config>>;

    fn next_event<'a>(
        &'a mut self,
        bx: &'a mut ReusableBox,
    ) -> ReusedBoxFuture<'a, Result<SourceEvent<Self::MediaType>>>;
}

downcast_rs::impl_downcast!(DynSource assoc MediaType);

impl<S: Source> DynSource for S {
    type MediaType = S::MediaType;

    fn capabilities<'a>(
        &'a mut self,
        bx: &'a mut ReusableBox,
    ) -> ReusedBoxFuture<'a, Result<Vec<<Self::MediaType as MediaType>::ConfigRange>>> {
        bx.store_future(Source::capabilities(self))
    }

    fn negotiate_config<'a>(
        &'a mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
        bx: &'a mut ReusableBox,
    ) -> ReusedBoxFuture<'a, Result<<Self::MediaType as MediaType>::Config>> {
        bx.store_future(Source::negotiate_config(self, available))
    }

    fn next_event<'a>(
        &'a mut self,
        bx: &'a mut ReusableBox,
    ) -> ReusedBoxFuture<'a, Result<SourceEvent<Self::MediaType>>> {
        bx.store_future(Source::next_event(self))
    }
}

/// [`BoxedSource`] with NextEventIsCancelSafe implemented
pub struct BoxedSourceCancelSafe<M: MediaType>(BoxedSource<M>);

impl<M: MediaType> NextEventIsCancelSafe for BoxedSourceCancelSafe<M> {}

impl<M: MediaType> BoxedSourceCancelSafe<M> {
    #[inline]
    pub fn new(source: impl Source<MediaType = M> + NextEventIsCancelSafe) -> Self {
        Self(source.boxed())
    }

    #[inline]
    pub fn downcast<T: Source<MediaType = M>>(self) -> Result<Box<T>, Self> {
        self.0.downcast().map_err(Self)
    }

    #[inline]
    pub fn downcast_ref<T: Source<MediaType = M>>(&self) -> Option<&T> {
        self.0.downcast_ref()
    }

    #[inline]
    pub fn downcast_mut<T: Source<MediaType = M>>(&mut self) -> Option<&mut T> {
        self.0.downcast_mut()
    }
}

impl<M: MediaType> Source for BoxedSourceCancelSafe<M> {
    type MediaType = M;

    async fn capabilities(&mut self) -> Result<Vec<<Self::MediaType as MediaType>::ConfigRange>> {
        Source::capabilities(&mut self.0).await
    }

    async fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
    ) -> Result<<Self::MediaType as MediaType>::Config> {
        Source::negotiate_config(&mut self.0, available).await
    }

    async fn next_event(&mut self) -> Result<SourceEvent<Self::MediaType>> {
        Source::next_event(&mut self.0).await
    }

    fn boxed(self) -> BoxedSource<Self::MediaType> {
        self.0
    }

    fn boxed_cancel_safe(self) -> BoxedSourceCancelSafe<Self::MediaType>
    where
        Self: NextEventIsCancelSafe,
    {
        self
    }
}

/// Wraps a [`BoxedSource`], implements [`Future`] and [`Stream`] yielding the result of [`Source::next_event`]
///
/// The `Stream` implementation recalls `next_event` after returning an item, while the `Future` does not.
pub struct SourceStream<M: MediaType>(Option<SourceStreamImpl<M>>);

impl<M: MediaType> SourceStream<M> {
    pub fn new(source: impl Source<MediaType = M>) -> Self {
        let source = source.boxed();

        Self(Some(new_impl(source)))
    }

    pub fn into_inner(self) -> BoxedSource<M> {
        self.0.unwrap().into_heads().source
    }
}

fn new_impl<M: MediaType>(source: BoxedSource<M>) -> SourceStreamImpl<M> {
    SourceStreamImplBuilder {
        source,
        next_event_future_builder: |boxed_source| {
            boxed_source
                .source
                .next_event(&mut boxed_source.reusable_box)
        },
    }
    .build()
}

impl<M: MediaType> Future for SourceStream<M> {
    type Output = Result<SourceEvent<M>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0
            .as_mut()
            .unwrap()
            .with_mut(|mut this| Pin::new(&mut this.next_event_future).poll(cx))
    }
}

impl<M: MediaType> Stream for SourceStream<M> {
    type Item = Result<SourceEvent<M>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.as_mut().poll(cx) {
            Poll::Ready(r) => {
                let boxed_source = take(&mut self.0).unwrap().into_heads().source;

                self.0 = Some(new_impl(boxed_source));

                Poll::Ready(Some(r))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

#[self_referencing]
struct SourceStreamImpl<M: MediaType> {
    source: BoxedSource<M>,
    #[borrows(mut source)]
    #[covariant]
    next_event_future: ReusedBoxFuture<'this, Result<SourceEvent<M>>>,
}
