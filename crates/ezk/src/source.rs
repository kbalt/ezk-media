use crate::{Frame, MediaType, Result};
use downcast_rs::Downcast;
use reusable_box::{ReusableBox, ReusedBoxFuture};
use std::future::Future;

#[diagnostic::on_unimplemented(message = "Try wrapping the source in ezk::Tasked")]
pub trait NextEventIsCancelSafe {}

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
    /// This method is __never__ cancel safe. Cancelling it may leave some sources in the stack configured and others not.
    fn negotiate_config(
        &mut self,
        available: Vec<<Self::MediaType as MediaType>::ConfigRange>,
    ) -> impl Future<Output = Result<<Self::MediaType as MediaType>::Config>> + Send;

    /// Fetch the next even from the source. This method should be called as much as possible to
    /// allow source to drive their internal logic without having to rely on extra tasks.
    ///
    /// Should not be called before successfully negotiating a config with [`Source::negotiate_config`]. It is valid for
    /// implementations to assume that this method will never be called before negotiating.
    ///
    /// # Cancel safety
    ///
    /// The cancel safety of this method depends on the implementor but usually isn't.
    ///
    /// Wrap sources in [`Tasked`](crate::ng_nodes::Tasked) to guarantee cancel safety.
    fn next_event(&mut self) -> impl Future<Output = Result<SourceEvent<Self::MediaType>>> + Send;

    /// Erase the type of this node
    fn boxed(self) -> BoxedSource<Self::MediaType> {
        BoxedSource {
            source: Box::new(self),
            reusable_box: ReusableBox::new(),
        }
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
