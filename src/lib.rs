use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut},
    pin::Pin,
    task::{Context, Poll},
};

use abi_stable::{
    extern_fn_panic_handling, sabi_extern_fn,
    std_types::{ROption, RResult, Tuple2},
    StableAbi,
};
use async_ffi::{ContextExt, FfiContext, FfiPoll};
use futures_core::{Stream, TryStream};
use futures_sink::Sink;
use ruchei_route::RouteSink;

use self::auto::Auto;

mod auto;

#[repr(u8)]
#[derive(StableAbi)]
enum Started<E> {
    Ok,
    Err(E),
    Panicked,
}

struct StartPanicked;

impl<E> Started<E> {
    fn try_into_result(self) -> Result<Result<(), E>, StartPanicked> {
        match self {
            Started::Ok => Ok(Ok(())),
            Started::Err(e) => Ok(Err(e)),
            Started::Panicked => Err(StartPanicked),
        }
    }
}

type StreamPoll<T> = FfiPoll<ROption<T>>;

#[repr(C)]
#[derive(StableAbi)]
pub struct LocalBorrowingFfiStream<'a, T> {
    ptr: *mut (),
    drop_fn: unsafe extern "C" fn(*mut ()),
    next_fn: unsafe extern "C" fn(*mut (), *mut FfiContext) -> StreamPoll<T>,
    _marker: PhantomData<&'a ()>,
}

pub type LocalFfiStream<T> = LocalBorrowingFfiStream<'static, T>;

trait StreamStrategy<S> {
    type T;
    fn poll_next(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Option<Self::T>>;
}

struct StreamImpl;

impl<S: Stream> StreamStrategy<S> for StreamImpl {
    type T = S::Item;

    fn poll_next(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Option<Self::T>> {
        target.poll_next(cx)
    }
}

struct TryStreamImpl;

impl<S: TryStream> StreamStrategy<S> for TryStreamImpl {
    type T = RResult<S::Ok, S::Error>;

    fn poll_next(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Option<Self::T>> {
        target.try_poll_next(cx).auto()
    }
}

impl<'a, T> LocalBorrowingFfiStream<'a, T> {
    pub fn new<S: 'a + Stream<Item = T>>(itr: S) -> Self {
        let ptr = Box::into_raw(Box::new(itr));

        unsafe { Self::from_ptr::<_, StreamImpl>(ptr) }
    }

    unsafe fn from_ptr<S: 'a, X: StreamStrategy<S, T = T>>(ptr: *mut S) -> Self {
        #[sabi_extern_fn]
        unsafe fn drop_fn<S>(ptr: *mut ()) {
            drop(Box::from_raw(ptr.cast::<S>()));
        }

        unsafe extern "C" fn next_fn<S, X: StreamStrategy<S>>(
            ptr: *mut (),
            context_ptr: *mut FfiContext,
        ) -> StreamPoll<X::T> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                (*context_ptr).with_context(|cx| X::poll_next(pin, cx))
            }));
            match r {
                Ok(p) => p.auto(),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    FfiPoll::Panicked
                }
            }
        }

        Self {
            ptr: ptr.cast(),
            next_fn: next_fn::<_, X>,
            drop_fn: drop_fn::<S>,
            _marker: PhantomData,
        }
    }
}

impl<'a, In, E> LocalBorrowingFfiStream<'a, RResult<In, E>> {
    pub fn from_try_stream<S: 'a + TryStream<Ok = In, Error = E>>(itr: S) -> Self {
        let ptr = Box::into_raw(Box::new(itr));

        unsafe { Self::from_ptr::<_, TryStreamImpl>(ptr) }
    }
}

impl<T> Drop for LocalBorrowingFfiStream<'_, T> {
    fn drop(&mut self) {
        unsafe { (self.drop_fn)(self.ptr) }
    }
}

impl<T> Stream for LocalBorrowingFfiStream<'_, T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        cx.with_ffi_context(|cx| unsafe { (self.next_fn)(self.ptr, cx) })
            .auto()
    }
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct BorrowingFfiStream<'a, T>(LocalBorrowingFfiStream<'a, T>);

impl<'a, T> BorrowingFfiStream<'a, T> {
    pub fn new<S: 'a + Send + Stream<Item = T>>(itr: S) -> Self {
        Self(LocalBorrowingFfiStream::new(itr))
    }
}

impl<'a, In, E> BorrowingFfiStream<'a, RResult<In, E>> {
    pub fn from_try_stream<S: 'a + TryStream<Ok = In, Error = E>>(itr: S) -> Self {
        Self(LocalBorrowingFfiStream::from_try_stream(itr))
    }
}

unsafe impl<T> Send for BorrowingFfiStream<'_, T> {}

impl<T> Stream for BorrowingFfiStream<'_, T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0).poll_next(cx)
    }
}

type SinkPoll<E> = FfiPoll<RResult<(), E>>;

impl<'a, In, Out, E> Deref for LocalBorrowingFfiDealer<'a, In, Out, E> {
    type Target = LocalBorrowingFfiStream<'a, RResult<In, E>>;

    fn deref(&self) -> &Self::Target {
        &self.stream
    }
}

impl<In, Out, E> DerefMut for LocalBorrowingFfiDealer<'_, In, Out, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.stream
    }
}

impl<'a, In, Out, E> LocalBorrowingFfiDealer<'a, In, Out, E> {
    pub fn as_mut(self: Pin<&mut Self>) -> Pin<&mut LocalBorrowingFfiStream<'a, RResult<In, E>>> {
        Pin::new(self.get_mut())
    }
}

#[repr(C)]
#[derive(StableAbi)]
pub struct LocalBorrowingFfiDealer<'a, In, Out, E> {
    stream: LocalBorrowingFfiStream<'a, RResult<In, E>>,
    close_fn: unsafe extern "C" fn(*mut (), *mut FfiContext) -> SinkPoll<E>,
    start_fn: unsafe extern "C" fn(*mut (), Out) -> Started<E>,
    flush_fn: unsafe extern "C" fn(*mut (), *mut FfiContext) -> SinkPoll<E>,
    ready_fn: unsafe extern "C" fn(*mut (), *mut FfiContext) -> SinkPoll<E>,
}

pub type LocalFfiDealer<In, Out, E> = LocalBorrowingFfiDealer<'static, In, Out, E>;

trait DealerStrategy<S, Out>: StreamStrategy<S, T = RResult<Self::In, Self::E>> {
    type In;
    type E;
    fn poll_ready(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>>;
    fn start_send(target: Pin<&mut S>, item: Out) -> Result<(), Self::E>;
    fn poll_flush(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>>;
    fn poll_close(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>>;
}

struct DealerImpl;

impl<In, E, S: TryStream<Ok = In, Error = E>> StreamStrategy<S> for DealerImpl {
    type T = RResult<In, E>;

    fn poll_next(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Option<Self::T>> {
        target.try_poll_next(cx).auto()
    }
}

impl<In, Out, E, S: TryStream<Ok = In, Error = E> + Sink<Out, Error = E>> DealerStrategy<S, Out>
    for DealerImpl
{
    type In = In;
    type E = E;

    fn poll_ready(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>> {
        target.poll_ready(cx)
    }

    fn start_send(target: Pin<&mut S>, item: Out) -> Result<(), Self::E> {
        target.start_send(item)
    }

    fn poll_flush(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>> {
        target.poll_flush(cx)
    }

    fn poll_close(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>> {
        target.poll_close(cx)
    }
}

impl<'a, In, Out, E> LocalBorrowingFfiDealer<'a, In, Out, E> {
    pub fn new<S: 'a + TryStream<Ok = In, Error = E> + Sink<Out, Error = E>>(dlr: S) -> Self {
        let ptr = Box::into_raw(Box::new(dlr));

        unsafe { Self::from_ptr::<_, DealerImpl>(ptr) }
    }

    unsafe fn from_ptr<S: 'a, X: DealerStrategy<S, Out, In = In, E = E>>(ptr: *mut S) -> Self {
        unsafe extern "C" fn ready_fn<Out, S, X: DealerStrategy<S, Out>>(
            ptr: *mut (),
            context_ptr: *mut FfiContext,
        ) -> SinkPoll<X::E> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                (*context_ptr).with_context(|cx| X::poll_ready(pin, cx))
            }));
            match r {
                Ok(p) => p.auto(),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    FfiPoll::Panicked
                }
            }
        }

        unsafe extern "C" fn start_fn<Out, S, X: DealerStrategy<S, Out>>(
            ptr: *mut (),
            item: Out,
        ) -> Started<X::E> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                X::start_send(pin, item)
            }));
            match r {
                Ok(Ok(())) => Started::Ok,
                Ok(Err(e)) => Started::Err(e),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    Started::Panicked
                }
            }
        }

        unsafe extern "C" fn flush_fn<Out, S, X: DealerStrategy<S, Out>>(
            ptr: *mut (),
            context_ptr: *mut FfiContext,
        ) -> SinkPoll<X::E> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                (*context_ptr).with_context(|cx| X::poll_flush(pin, cx))
            }));
            match r {
                Ok(p) => p.auto(),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    FfiPoll::Panicked
                }
            }
        }

        unsafe extern "C" fn close_fn<Out, S, X: DealerStrategy<S, Out>>(
            ptr: *mut (),
            context_ptr: *mut FfiContext,
        ) -> SinkPoll<X::E> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                (*context_ptr).with_context(|cx| X::poll_close(pin, cx))
            }));
            match r {
                Ok(p) => p.auto(),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    FfiPoll::Panicked
                }
            }
        }

        Self {
            stream: LocalBorrowingFfiStream::from_ptr::<_, X>(ptr),
            ready_fn: ready_fn::<_, _, X>,
            start_fn: start_fn::<_, _, X>,
            flush_fn: flush_fn::<_, _, X>,
            close_fn: close_fn::<_, _, X>,
        }
    }

    pub fn into_stream(self) -> LocalBorrowingFfiStream<'a, RResult<In, E>> {
        self.stream
    }
}

impl<In, Out, E> Stream for LocalBorrowingFfiDealer<'_, In, Out, E> {
    type Item = Result<In, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().poll_next(cx).auto()
    }
}

impl<In, Out, E> Sink<Out> for LocalBorrowingFfiDealer<'_, In, Out, E> {
    type Error = E;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        cx.with_ffi_context(|cx| unsafe { (self.ready_fn)(self.ptr, cx) })
            .auto()
    }

    fn start_send(self: Pin<&mut Self>, item: Out) -> Result<(), Self::Error> {
        unsafe { (self.start_fn)(self.ptr, item) }.auto()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        cx.with_ffi_context(|cx| unsafe { (self.flush_fn)(self.ptr, cx) })
            .auto()
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        cx.with_ffi_context(|cx| unsafe { (self.close_fn)(self.ptr, cx) })
            .auto()
    }
}

impl<'a, In, Out, E> Deref for BorrowingFfiDealer<'a, In, Out, E> {
    type Target = BorrowingFfiStream<'a, RResult<In, E>>;

    fn deref(&self) -> &Self::Target {
        unsafe { std::mem::transmute(&self.0.stream) }
    }
}

impl<In, Out, E> DerefMut for BorrowingFfiDealer<'_, In, Out, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { std::mem::transmute(&mut self.0.stream) }
    }
}

impl<'a, In, Out, E> BorrowingFfiDealer<'a, In, Out, E> {
    pub fn as_mut(self: Pin<&mut Self>) -> Pin<&mut BorrowingFfiStream<'a, RResult<In, E>>> {
        Pin::new(self.get_mut())
    }
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct BorrowingFfiDealer<'a, In, Out, E>(LocalBorrowingFfiDealer<'a, In, Out, E>);

pub type FfiDealer<In, Out, E> = BorrowingFfiDealer<'static, In, Out, E>;

impl<'a, In, Out, E> BorrowingFfiDealer<'a, In, Out, E> {
    pub fn new<S: 'a + Send + TryStream<Ok = In, Error = E> + Sink<Out, Error = E>>(
        dlr: S,
    ) -> Self {
        Self(LocalBorrowingFfiDealer::new(dlr))
    }

    pub fn into_stream(self) -> BorrowingFfiStream<'a, RResult<In, E>> {
        BorrowingFfiStream(self.0.into_stream())
    }
}

unsafe impl<In, Out, E> Send for BorrowingFfiDealer<'_, In, Out, E> {}

impl<In, Out, E> Stream for BorrowingFfiDealer<'_, In, Out, E> {
    type Item = Result<In, E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0).poll_next(cx)
    }
}

impl<In, Out, E> Sink<Out> for BorrowingFfiDealer<'_, In, Out, E> {
    type Error = E;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: Out) -> Result<(), Self::Error> {
        Pin::new(&mut self.get_mut().0).start_send(item)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_close(cx)
    }
}

type LocalRouterDealer<'a, K, M, Out, E> =
    LocalBorrowingFfiDealer<'a, Tuple2<K, M>, Tuple2<K, Out>, E>;

impl<'a, K, M, Out, E> Deref for LocalBorrowingFfiRouter<'a, K, M, Out, E> {
    type Target = LocalRouterDealer<'a, K, M, Out, E>;

    fn deref(&self) -> &Self::Target {
        &self.dealer
    }
}

impl<K, M, Out, E> DerefMut for LocalBorrowingFfiRouter<'_, K, M, Out, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.dealer
    }
}

impl<'a, K, M, Out, E> LocalBorrowingFfiRouter<'a, K, M, Out, E> {
    pub fn as_mut(self: Pin<&mut Self>) -> Pin<&mut LocalRouterDealer<'a, K, M, Out, E>> {
        Pin::new(self.get_mut())
    }
}

#[repr(C)]
#[derive(StableAbi)]
pub struct LocalBorrowingFfiRouter<'a, K, M, Out, E> {
    dealer: LocalRouterDealer<'a, K, M, Out, E>,
    is_routing: bool,
    ready_fn: Option<unsafe extern "C" fn(*mut (), *const K, *mut FfiContext) -> SinkPoll<E>>,
    flush_fn: Option<unsafe extern "C" fn(*mut (), *const K, *mut FfiContext) -> SinkPoll<E>>,
    _close_fn: Option<unsafe extern "C" fn(*mut (), *const K, *mut FfiContext) -> SinkPoll<E>>,
}

pub type LocalFfiRouter<K, In, Out, E> = LocalBorrowingFfiRouter<'static, K, In, Out, E>;

trait RouterStrategy<S, Out> {
    type K;
    type M;
    type E;
    type D: DealerStrategy<S, Tuple2<Self::K, Out>, In = Tuple2<Self::K, Self::M>, E = Self::E>;

    fn poll_ready(
        target: Pin<&mut S>,
        route: &Self::K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::E>>;
    fn poll_flush(
        target: Pin<&mut S>,
        route: &Self::K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::E>>;
}

struct RouterImpl;

impl<K, M, E, S: TryStream<Ok = (K, M), Error = E>> StreamStrategy<S> for RouterImpl {
    type T = RResult<Tuple2<K, M>, E>;

    fn poll_next(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Option<Self::T>> {
        target.try_poll_next(cx).auto()
    }
}

impl<K, M, Out, E, S: TryStream<Ok = (K, M), Error = E> + RouteSink<K, Out, Error = E>>
    DealerStrategy<S, Tuple2<K, Out>> for RouterImpl
{
    type In = Tuple2<K, M>;
    type E = E;

    fn poll_ready(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>> {
        target.poll_ready_any(cx)
    }

    fn start_send(target: Pin<&mut S>, Tuple2(route, msg): Tuple2<K, Out>) -> Result<(), Self::E> {
        target.start_send(route, msg)
    }

    fn poll_flush(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>> {
        target.poll_flush_all(cx)
    }

    fn poll_close(target: Pin<&mut S>, cx: &mut Context<'_>) -> Poll<Result<(), Self::E>> {
        target.poll_close(cx)
    }
}

impl<K, M, Out, E, S: TryStream<Ok = (K, M), Error = E> + RouteSink<K, Out, Error = E>>
    RouterStrategy<S, Out> for RouterImpl
{
    type K = K;
    type M = M;
    type D = Self;
    type E = E;

    fn poll_ready(
        target: Pin<&mut S>,
        route: &Self::K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::E>> {
        target.poll_ready(route, cx)
    }

    fn poll_flush(
        target: Pin<&mut S>,
        route: &Self::K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::E>> {
        target.poll_flush(route, cx)
    }
}

impl<'a, K, M, Out, E> LocalBorrowingFfiRouter<'a, K, M, Out, E> {
    pub fn new<S: 'a + TryStream<Ok = (K, M), Error = E> + RouteSink<K, Out, Error = E>>(
        rtr: S,
    ) -> Self {
        let is_routing = rtr.is_routing();

        let ptr = Box::into_raw(Box::new(rtr));

        unsafe { Self::from_ptr::<_, RouterImpl>(ptr, is_routing) }
    }

    unsafe fn from_ptr<S: 'a, X: RouterStrategy<S, Out, K = K, M = M, E = E>>(
        ptr: *mut S,
        is_routing: bool,
    ) -> Self {
        unsafe extern "C" fn ready_fn<K, Out, S, X: RouterStrategy<S, Out, K = K>>(
            ptr: *mut (),
            route: *const K,
            context_ptr: *mut FfiContext,
        ) -> SinkPoll<X::E> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                (*context_ptr).with_context(|cx| X::poll_ready(pin, &*route, cx))
            }));
            match r {
                Ok(p) => p.auto(),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    FfiPoll::Panicked
                }
            }
        }

        unsafe extern "C" fn flush_fn<K, Out, S, X: RouterStrategy<S, Out, K = K>>(
            ptr: *mut (),
            route: *const K,
            context_ptr: *mut FfiContext,
        ) -> SinkPoll<X::E> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let pin = Pin::new_unchecked(&mut *ptr.cast::<S>());
                (*context_ptr).with_context(|cx| X::poll_flush(pin, &*route, cx))
            }));
            match r {
                Ok(p) => p.auto(),
                Err(payload) => {
                    extern_fn_panic_handling! {
                        drop(payload);
                    };
                    FfiPoll::Panicked
                }
            }
        }

        Self {
            dealer: LocalBorrowingFfiDealer::from_ptr::<_, X::D>(ptr),
            is_routing,
            ready_fn: (is_routing).then_some(ready_fn::<_, _, _, X>),
            flush_fn: (is_routing).then_some(flush_fn::<_, _, _, X>),
            _close_fn: (is_routing).then_some(flush_fn::<_, _, _, X>),
        }
    }

    pub fn into_stream(self) -> LocalBorrowingFfiStream<'a, RResult<Tuple2<K, M>, E>> {
        self.dealer.stream
    }

    pub fn into_dealer(self) -> LocalRouterDealer<'a, K, M, Out, E> {
        assert!(!self.is_routing);
        assert!(self.ready_fn.is_none());
        assert!(self.flush_fn.is_none());
        self.dealer
    }

    pub fn from_dealer(dealer: LocalRouterDealer<'a, K, M, Out, E>) -> Self {
        Self {
            dealer,
            is_routing: false,
            ready_fn: None,
            flush_fn: None,
            _close_fn: None,
        }
    }
}

impl<K, M, Out, E> Stream for LocalBorrowingFfiRouter<'_, K, M, Out, E> {
    type Item = Result<(K, M), E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().poll_next(cx).auto()
    }
}

impl<K, M, Out, E> RouteSink<K, Out> for LocalBorrowingFfiRouter<'_, K, M, Out, E> {
    type Error = E;

    fn poll_ready(
        self: Pin<&mut Self>,
        route: &K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if let Some(ready_fn) = self.ready_fn {
            cx.with_ffi_context(|cx| unsafe { ready_fn(self.ptr, route, cx) })
                .auto()
        } else {
            self.poll_ready_any(cx)
        }
    }

    fn poll_ready_any(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.as_mut().poll_ready(cx)
    }

    fn start_send(self: Pin<&mut Self>, route: K, msg: Out) -> Result<(), Self::Error> {
        self.as_mut().start_send(Tuple2(route, msg))
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        route: &K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        if let Some(flush_fn) = self.flush_fn {
            cx.with_ffi_context(|cx| unsafe { flush_fn(self.ptr, route, cx) })
                .auto()
        } else {
            self.poll_flush_all(cx)
        }
    }

    fn poll_flush_all(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.as_mut().poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.as_mut().poll_close(cx)
    }

    fn is_routing(&self) -> bool {
        self.is_routing
    }
}

type RouterDealer<'a, K, M, Out, E> = BorrowingFfiDealer<'a, Tuple2<K, M>, Tuple2<K, Out>, E>;

impl<'a, K, M, Out, E> Deref for BorrowingFfiRouter<'a, K, M, Out, E> {
    type Target = RouterDealer<'a, K, M, Out, E>;

    fn deref(&self) -> &Self::Target {
        unsafe { std::mem::transmute(&self.0.dealer) }
    }
}

impl<K, M, Out, E> DerefMut for BorrowingFfiRouter<'_, K, M, Out, E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { std::mem::transmute(&mut self.0.dealer) }
    }
}

impl<'a, K, M, Out, E> BorrowingFfiRouter<'a, K, M, Out, E> {
    pub fn as_mut(self: Pin<&mut Self>) -> Pin<&mut RouterDealer<'a, K, M, Out, E>> {
        Pin::new(self.get_mut())
    }
}

#[repr(transparent)]
#[derive(StableAbi)]
pub struct BorrowingFfiRouter<'a, K, M, Out, E>(LocalBorrowingFfiRouter<'a, K, M, Out, E>);

pub type FfiRouter<K, M, Out, E> = BorrowingFfiRouter<'static, K, M, Out, E>;

impl<'a, K, M, Out, E> BorrowingFfiRouter<'a, K, M, Out, E> {
    pub fn new<S: 'a + Send + TryStream<Ok = (K, M), Error = E> + RouteSink<K, Out, Error = E>>(
        rtr: S,
    ) -> Self {
        Self(LocalBorrowingFfiRouter::new(rtr))
    }

    pub fn into_stream(self) -> BorrowingFfiStream<'a, RResult<Tuple2<K, M>, E>> {
        BorrowingFfiStream(self.0.into_stream())
    }

    pub fn into_dealer(self) -> BorrowingFfiDealer<'a, Tuple2<K, M>, Tuple2<K, Out>, E> {
        BorrowingFfiDealer(self.0.into_dealer())
    }

    pub fn from_dealer(dealer: BorrowingFfiDealer<'a, Tuple2<K, M>, Tuple2<K, Out>, E>) -> Self {
        Self(LocalBorrowingFfiRouter::from_dealer(dealer.0))
    }
}

unsafe impl<K, M, Out, E> Send for BorrowingFfiRouter<'_, K, M, Out, E> {}

impl<K, M, Out, E> Stream for BorrowingFfiRouter<'_, K, M, Out, E> {
    type Item = Result<(K, M), E>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().0).poll_next(cx)
    }
}

impl<K, M, Out, E> RouteSink<K, Out> for BorrowingFfiRouter<'_, K, M, Out, E> {
    type Error = E;

    fn poll_ready(
        self: Pin<&mut Self>,
        route: &K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_ready(route, cx)
    }

    fn poll_ready_any(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_ready_any(cx)
    }

    fn start_send(self: Pin<&mut Self>, route: K, item: Out) -> Result<(), Self::Error> {
        Pin::new(&mut self.get_mut().0).start_send(route, item)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        route: &K,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_flush(route, cx)
    }

    fn poll_flush_all(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_flush_all(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Pin::new(&mut self.get_mut().0).poll_close(cx)
    }

    fn is_routing(&self) -> bool {
        self.0.is_routing()
    }
}
