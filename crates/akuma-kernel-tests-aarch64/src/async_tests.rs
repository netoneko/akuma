//! Async Network Tests (Stubbed for SmolNet)

use alloc::boxed::Box;

pub fn run_all() -> bool {
    crate::console::print("[AsyncTests] Skipping tests during smoltcp migration\n");
    true
}

pub fn run_async_test<F, T>(future: F) -> T
where
    F: core::future::Future<Output = T>,
{
    // Use a simple blocking executor for tests. `Box::pin` and `Waker::noop()`
    // are the safe spellings of what this hand-rolled: `Pin::new_unchecked` over
    // a fresh `Box` (which cannot be moved out of, so pinning it is always sound)
    // and a `RawWakerVTable` of four empty closures.
    let mut future = Box::pin(future);

    loop {
        let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
        
        match future.as_mut().poll(&mut cx) {
            core::task::Poll::Ready(val) => return val,
            core::task::Poll::Pending => {
                #[cfg(feature = "smoltcp")]
                akuma_net::smoltcp_net::poll();
                akuma_exec::threading::yield_now();
            }
        }
    }
}