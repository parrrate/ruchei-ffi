use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};

use ruchei_ffi_context::FutureExt;

struct UwU {
    ctr: usize,
}

impl Future for UwU {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.ctr > 0 {
            self.ctr -= 1;
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    }
}

struct OwO<T>(T);

impl<T: Unpin + Future> Future for OwO<T> {
    type Output = T::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            match Pin::new(&mut self.0).poll(cx) {
                Poll::Ready(x) => break Poll::Ready(x),
                Poll::Pending => continue,
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let start = Instant::now();
    OwO(UwU { ctr: 100_000_000 }.into_ffi()).await;
    println!("{}", start.elapsed().as_millis());
}
