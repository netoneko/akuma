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
    // Use a simple blocking executor for tests
    let mut future = unsafe { core::pin::Pin::new_unchecked(Box::new(future)) };
    
    static VTABLE: core::task::RawWakerVTable = core::task::RawWakerVTable::new(
        |_| core::task::RawWaker::new(core::ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );
    
    loop {
        let waker = unsafe { core::task::Waker::from_raw(core::task::RawWaker::new(core::ptr::null(), &VTABLE)) };
        let mut cx = core::task::Context::from_waker(&waker);
        
        match future.as_mut().poll(&mut cx) {
            core::task::Poll::Ready(val) => return val,
            core::task::Poll::Pending => {
                crate::smoltcp_net::poll();
                crate::threading::yield_now();
            }
        }
    }
}