//! `ruchei`'s worse version of `async-ffi`
//!
//! Based on <https://docs.rs/async-ffi/0.5.0/src/async_ffi/lib.rs.html>
//!
//! This is in an attempt to reduce allocations and improve waker equality.
//! Technically, this crate achieves that, but at a very high performance cost.

use std::{
    cell::RefCell,
    future::Future,
    marker::PhantomData,
    mem::ManuallyDrop,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
};

use abi_stable::StableAbi;

pub const ABI_VERSION: u32 = 2;

#[repr(C, u8)]
#[derive(StableAbi)]
pub enum FfiPoll<T> {
    Ready(T),
    Pending,
    Panicked,
}

#[repr(C)]
#[derive(StableAbi)]
pub struct FfiContext<'a> {
    waker: *const FfiWaker,
    _marker: PhantomData<&'a mut ()>,
}

impl FfiContext<'_> {
    unsafe fn new(waker: *const FfiWaker) -> Self {
        Self {
            waker,
            _marker: PhantomData,
        }
    }

    pub fn with_context<T, F: FnOnce(&mut Context) -> T>(&mut self, f: F) -> T {
        static RUST_WAKER_VTABLE: RawWakerVTable = {
            unsafe fn clone(data: *const ()) -> RawWaker {
                let waker = data.cast::<FfiWaker>();
                let cloned = ((*waker).vtable.clone)(waker.cast());
                RawWaker::new(cloned.cast(), &RUST_WAKER_VTABLE)
            }
            unsafe fn wake(data: *const ()) {
                let waker = data.cast::<FfiWaker>();
                ((*waker).vtable.wake)(waker.cast());
            }
            unsafe fn wake_by_ref(data: *const ()) {
                let waker = data.cast::<FfiWaker>();
                ((*waker).vtable.wake_by_ref)(waker.cast());
            }
            unsafe fn drop(data: *const ()) {
                let waker = data.cast::<FfiWaker>();
                ((*waker).vtable.drop)(waker.cast());
            }
            RawWakerVTable::new(clone, wake, wake_by_ref, drop)
        };
        let waker = unsafe { ManuallyDrop::new(Waker::new(self.waker.cast(), &RUST_WAKER_VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        f(&mut cx)
    }
}

pub trait ContextExt {
    fn with_ffi_context<T, F: FnOnce(&mut FfiContext) -> T>(&mut self, f: F) -> T;
}

impl ContextExt for Context<'_> {
    fn with_ffi_context<T, F: FnOnce(&mut FfiContext) -> T>(&mut self, f: F) -> T {
        thread_local! {
            static INNER: RefCell<Arc<FfiWakerInner>> = RefCell::new(Arc::new(FfiWakerInner::NOOP));
        };
        fn new_waker(wptr: *const (), vtable: &'static RawWakerVTable) -> *const FfiWakerInner {
            INNER.with_borrow_mut(|inner| {
                {
                    let inner = Arc::make_mut(inner);
                    inner.base.vtable = &C_WAKER_VTABLE_ARC;
                    inner.wptr = wptr;
                    inner.vtable = vtable;
                }
                Arc::as_ptr(inner)
            })
            // Arc::into_raw(Arc::new(FfiWakerInner {
            //     base: FfiWaker {
            //         vtable: &C_WAKER_VTABLE_ARC,
            //     },
            //     wptr,
            //     vtable,
            // }))
        }

        static C_WAKER_VTABLE_ARC: FfiWakerVTable = {
            unsafe extern "C" fn clone(data: *const FfiWaker) -> *const FfiWaker {
                let data = data.cast::<FfiWakerInner>();
                let waker = Waker::new((*data).wptr, (*data).vtable);
                let cloned = waker.clone();
                let same = waker.will_wake(&cloned);
                std::mem::forget(waker);
                let wptr = cloned.data();
                let vtable = cloned.vtable();
                std::mem::forget(cloned);
                let data = if same {
                    Arc::increment_strong_count(data);
                    data
                } else {
                    new_waker(wptr, vtable)
                };
                data.cast()
            }
            unsafe extern "C" fn wake(data: *const FfiWaker) {
                let data = data.cast::<FfiWakerInner>();
                Waker::new((*data).wptr, (*data).vtable).wake();
                Arc::decrement_strong_count(data);
            }
            unsafe extern "C" fn wake_by_ref(data: *const FfiWaker) {
                let data = data.cast::<FfiWakerInner>();
                let waker = Waker::new((*data).wptr, (*data).vtable);
                waker.wake_by_ref();
                std::mem::forget(waker);
            }
            unsafe extern "C" fn drop(data: *const FfiWaker) {
                let data = data.cast::<FfiWakerInner>();
                let _ = Waker::new((*data).wptr, (*data).vtable);
                Arc::decrement_strong_count(data);
            }
            FfiWakerVTable {
                clone,
                wake,
                wake_by_ref,
                drop,
            }
        };
        let waker = self.waker();
        let wptr = waker.data();
        let vtable = waker.vtable();
        let data = new_waker(wptr, vtable);
        unsafe { Arc::increment_strong_count(data) };
        let mut cx = unsafe { FfiContext::new(data.cast()) };
        let value = { f(&mut cx) };
        unsafe { Arc::decrement_strong_count(data) };
        value
    }
}

#[repr(C, align(16))]
#[derive(StableAbi)]
struct FfiWaker {
    vtable: &'static FfiWakerVTable,
}

impl FfiWaker {
    const NOOP: Self = FfiWaker {
        vtable: &FfiWakerVTable::NOOP,
    };
}

#[repr(C)]
#[derive(StableAbi)]
struct FfiWakerVTable {
    clone: unsafe extern "C" fn(*const FfiWaker) -> *const FfiWaker,
    wake: unsafe extern "C" fn(*const FfiWaker),
    wake_by_ref: unsafe extern "C" fn(*const FfiWaker),
    drop: unsafe extern "C" fn(*const FfiWaker),
}

impl FfiWakerVTable {
    const NOOP: Self = const {
        unsafe extern "C" fn clone(_: *const FfiWaker) -> *const FfiWaker {
            std::ptr::null()
        }
        unsafe extern "C" fn wake(_: *const FfiWaker) {}
        unsafe extern "C" fn wake_by_ref(_: *const FfiWaker) {}
        unsafe extern "C" fn drop(_: *const FfiWaker) {}
        FfiWakerVTable {
            clone,
            wake,
            wake_by_ref,
            drop,
        }
    };
}

#[repr(C)]
struct FfiWakerInner {
    base: FfiWaker,
    wptr: *const (),
    vtable: &'static RawWakerVTable,
}

unsafe impl Send for FfiWakerInner {}
unsafe impl Sync for FfiWakerInner {}

impl Clone for FfiWakerInner {
    fn clone(&self) -> Self {
        Self {
            base: FfiWaker {
                vtable: self.base.vtable,
            },
            wptr: self.wptr,
            vtable: self.vtable,
        }
    }
}

const NOOP_VTABLE: RawWakerVTable = const {
    unsafe fn clone(_: *const ()) -> RawWaker {
        RawWaker::new(std::ptr::null(), &NOOP_VTABLE)
    }
    unsafe fn wake(_: *const ()) {}
    unsafe fn wake_by_ref(_: *const ()) {}
    unsafe fn drop(_: *const ()) {}
    RawWakerVTable::new(clone, wake, wake_by_ref, drop)
};

impl FfiWakerInner {
    const NOOP: Self = FfiWakerInner {
        base: FfiWaker::NOOP,
        wptr: std::ptr::null(),
        vtable: &NOOP_VTABLE,
    };
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct BorrowingFfiFuture<'a, T>(LocalBorrowingFfiFuture<'a, T>);

pub type FfiFuture<T> = BorrowingFfiFuture<'static, T>;

pub trait FutureExt: Future + Sized {
    fn into_ffi<'a>(self) -> BorrowingFfiFuture<'a, Self::Output>
    where
        Self: Send + 'a,
    {
        BorrowingFfiFuture::new(self)
    }

    fn into_local_ffi<'a>(self) -> LocalBorrowingFfiFuture<'a, Self::Output>
    where
        Self: 'a,
    {
        LocalBorrowingFfiFuture::new(self)
    }
}

impl<F> FutureExt for F where F: Future + Sized {}

#[derive(Debug)]
pub struct PollPanicked {
    _private: (),
}

impl std::fmt::Display for PollPanicked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FFI poll function panicked")
    }
}

impl std::error::Error for PollPanicked {}

impl<T> FfiPoll<T> {
    pub fn from_poll(poll: Poll<T>) -> Self {
        match poll {
            Poll::Ready(r) => Self::Ready(r),
            Poll::Pending => Self::Pending,
        }
    }

    pub fn try_into_poll(self) -> Result<Poll<T>, PollPanicked> {
        match self {
            Self::Ready(r) => Ok(Poll::Ready(r)),
            Self::Pending => Ok(Poll::Pending),
            Self::Panicked => Err(PollPanicked { _private: () }),
        }
    }
}

impl<T> From<Poll<T>> for FfiPoll<T> {
    fn from(poll: Poll<T>) -> Self {
        Self::from_poll(poll)
    }
}

impl<T> TryFrom<FfiPoll<T>> for Poll<T> {
    type Error = PollPanicked;

    fn try_from(ffi_poll: FfiPoll<T>) -> Result<Self, PollPanicked> {
        ffi_poll.try_into_poll()
    }
}

impl<'a, T> BorrowingFfiFuture<'a, T> {
    pub fn new<F: Future<Output = T> + Send + 'a>(fut: F) -> Self {
        Self(LocalBorrowingFfiFuture::new(fut))
    }
}

unsafe impl<T> Send for BorrowingFfiFuture<'_, T> {}

impl<T> Future for BorrowingFfiFuture<'_, T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(ctx)
    }
}

#[repr(C)]
#[derive(StableAbi)]
pub struct LocalBorrowingFfiFuture<'a, T> {
    fut_ptr: *mut (),
    poll_fn: unsafe extern "C" fn(fut_ptr: *mut (), context_ptr: *mut FfiContext) -> FfiPoll<T>,
    drop_fn: unsafe extern "C" fn(*mut ()),
    _marker: PhantomData<&'a ()>,
}

pub type LocalFfiFuture<T> = LocalBorrowingFfiFuture<'static, T>;

impl<'a, T> LocalBorrowingFfiFuture<'a, T> {
    pub fn new<F: Future<Output = T> + 'a>(fut: F) -> Self {
        unsafe extern "C" fn poll_fn<F: Future>(
            fut_ptr: *mut (),
            context_ptr: *mut FfiContext,
        ) -> FfiPoll<F::Output> {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let fut_pin = Pin::new_unchecked(&mut *fut_ptr.cast::<F>());
                (*context_ptr).with_context(|ctx| F::poll(fut_pin, ctx))
            })) {
                Ok(p) => p.into(),
                Err(payload) => {
                    let _ = payload;
                    FfiPoll::Panicked
                }
            }
        }

        unsafe extern "C" fn drop_fn<T>(ptr: *mut ()) {
            let _ = Box::from_raw(ptr.cast::<T>());
        }

        let ptr = Box::into_raw(Box::new(fut));
        Self {
            fut_ptr: ptr.cast(),
            poll_fn: poll_fn::<F>,
            drop_fn: drop_fn::<F>,
            _marker: PhantomData,
        }
    }
}

impl<T> Drop for LocalBorrowingFfiFuture<'_, T> {
    fn drop(&mut self) {
        unsafe { (self.drop_fn)(self.fut_ptr) };
    }
}

impl<T> Future for LocalBorrowingFfiFuture<'_, T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        ctx.with_ffi_context(|ctx| unsafe { (self.poll_fn)(self.fut_ptr, ctx) })
            .try_into()
            .unwrap_or_else(|_| panic!("FFI future panicked"))
    }
}
