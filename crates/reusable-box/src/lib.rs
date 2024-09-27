use std::alloc::Layout;
use std::future::Future;
use std::mem::size_of;
use std::pin::Pin;
use std::ptr::{drop_in_place, NonNull};
use std::task::{Context, Poll};

#[derive(Default)]
pub struct ReusableBox {
    // using vec as convenient memory allocator
    buffer: Vec<u8>,
}

impl ReusableBox {
    pub const fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    pub fn store_future<'a, F, O>(&'a mut self, f: F) -> ReusedBoxFuture<'a, O>
    where
        F: Future<Output = O> + Send + 'a,
    {
        const USIZE_SIZE: usize = size_of::<usize>();

        let layout = Layout::new::<F>();

        // Make sure the buffer has the required size (+ size of usize for potential alignment)
        self.buffer.reserve(layout.size() + USIZE_SIZE);

        let align_offset = self.buffer.as_ptr().align_offset(layout.align());

        assert!(
            align_offset <= USIZE_SIZE,
            "Didn't expect the offset to be larger than {USIZE_SIZE} (is {align_offset})"
        );

        unsafe {
            let ptr = self.buffer.as_mut_ptr().add(align_offset).cast::<F>();

            ptr.write(f);

            // Cast ptr to dyn Future which can be used later to access and drop the future without any generic parameters
            let ptr = NonNull::new_unchecked(ptr as *mut (dyn Future<Output = O> + Send + 'a));

            ReusedBoxFuture {
                ptr_into_buffer: ptr,
            }
        }
    }
}

pub struct ReusedBoxFuture<'a, O> {
    ptr_into_buffer: NonNull<(dyn Future<Output = O> + Send + 'a)>,
}

// SAFETY:
//
// The future stored must be Send
unsafe impl<O: Send> Send for ReusedBoxFuture<'_, O> {}

impl<'a, O> ReusedBoxFuture<'a, O> {
    fn future(&mut self) -> Pin<&mut (dyn Future<Output = O> + Send + 'a)> {
        // SAFETY:
        // self.ptr_into_buffer must always point into a space allocated by a vec
        // Neither the pointer nor the vec which allocated the memory cannot be modified
        // while `ReusedBoxFuture` exists.
        unsafe { Pin::new_unchecked(self.ptr_into_buffer.as_mut()) }
    }
}

impl<O> Future for ReusedBoxFuture<'_, O> {
    type Output = O;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future().poll(cx)
    }
}

impl<O> Drop for ReusedBoxFuture<'_, O> {
    fn drop(&mut self) {
        // SAFETY:
        // ReusedBoxFuture's contract for creation requires the pointer to be valid
        unsafe {
            drop_in_place(self.ptr_into_buffer.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn set() {
        let mut holder = ReusableBox::new();

        let mut x = 0;

        for i in 0..10 {
            let g = holder.store_future(async {
                x += i;
            });

            g.await
        }

        assert_eq!(x, 1 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9);
    }

    struct OnDrop<F: FnOnce()>(Option<F>);

    impl<F: FnOnce()> OnDrop<F> {
        fn new(f: F) -> Self {
            Self(Some(f))
        }
    }

    impl<F: FnOnce()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }

    impl<F: FnOnce()> Future for OnDrop<F> {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(())
        }
    }

    #[tokio::test]
    async fn drop_future() {
        let mut holder = ReusableBox::new();

        let mut x = 0;

        for i in 0..10 {
            let g = holder.store_future(OnDrop::new(|| x += i));

            g.await
        }

        assert_eq!(x, 45);
    }

    #[tokio::test]
    async fn drop_on_set() {
        let mut holder = ReusableBox::new();

        let mut x = 0;

        holder.store_future(OnDrop::new(|| x += 1));
        let g = holder.store_future(OnDrop::new(|| ()));

        drop(g);
        drop(holder);

        assert_eq!(x, 1);
    }

    #[tokio::test]
    async fn return_value() {
        let mut holder = ReusableBox::new();

        holder.store_future(async { 1 });
        let g = holder.store_future(async { 2 });

        let v = g.await;

        assert_eq!(v, 2);
    }

    trait MyAsyncTrait {
        fn test(&mut self) -> impl Future<Output = u32> + Send;
    }

    impl MyAsyncTrait for u32 {
        async fn test(&mut self) -> u32 {
            *self + 2
        }
    }

    trait DynMyAsyncTrait: MyAsyncTrait {
        fn dyn_test<'this>(
            &'this mut self,
            bx: &'this mut ReusableBox,
        ) -> ReusedBoxFuture<'this, u32> {
            bx.store_future(MyAsyncTrait::test(self))
        }
    }

    impl DynMyAsyncTrait for u32 {}

    #[tokio::test]
    async fn async_trait_without_realloc() {
        let mut holder = ReusableBox::new();

        let mut xy = 1u32;
        let mut g = xy.dyn_test(&mut holder);

        let v = g.future().await;

        assert_eq!(v, 3);
    }
}
