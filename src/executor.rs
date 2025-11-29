use alloc::boxed::Box;
use alloc::vec::Vec;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use spinning_top::Spinlock;

static EXECUTOR: Spinlock<Option<Executor>> = Spinlock::new(None);

pub struct Executor {
    task_queue: Vec<Task>, // Changed from VecDeque to Vec
}

struct Task {
    future: Pin<Box<dyn Future<Output = ()> + Send>>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            task_queue: Vec::new(),
        }
    }

    pub fn spawn(&mut self, future: impl Future<Output = ()> + Send + 'static) {
        let task = Task {
            future: Box::pin(future),
        };
        self.task_queue.push(task);
    }

    pub fn run(&mut self) {
        while !self.task_queue.is_empty() {
            let mut task = self.task_queue.remove(0);
            let waker = dummy_waker();
            let mut context = Context::from_waker(&waker);

            match task.future.as_mut().poll(&mut context) {
                Poll::Ready(()) => {
                    // Task completed
                }
                Poll::Pending => {
                    // Re-queue the task
                    self.task_queue.push(task);
                }
            }
        }
    }

    pub fn run_once(&mut self) -> bool {
        if self.task_queue.is_empty() {
            return false;
        }

        // Poll all tasks in round-robin fashion without removing/moving them
        // This avoids the Vec::remove() hang issue
        let waker = dummy_waker();
        let mut context = Context::from_waker(&waker);

        let mut completed_indices = Vec::new();

        for (i, task) in self.task_queue.iter_mut().enumerate() {
            match task.future.as_mut().poll(&mut context) {
                Poll::Ready(()) => {
                    completed_indices.push(i);
                }
                Poll::Pending => {
                    // Task still pending, leave it in the queue
                }
            }
        }

        // Remove completed tasks in reverse order to avoid index shifting issues
        for i in completed_indices.iter().rev() {
            self.task_queue.swap_remove(*i);
        }

        !self.task_queue.is_empty()
    }

    pub fn has_tasks(&self) -> bool {
        !self.task_queue.is_empty()
    }
}

pub fn init() {
    let mut executor_lock = EXECUTOR.lock();
    *executor_lock = Some(Executor::new());
}

pub fn spawn(future: impl Future<Output = ()> + Send + 'static) {
    let mut executor_lock = EXECUTOR.lock();
    if let Some(executor) = executor_lock.as_mut() {
        executor.spawn(future);
    }
}

pub fn run_once() -> bool {
    let mut executor_lock = EXECUTOR.lock();
    if let Some(executor) = executor_lock.as_mut() {
        executor.run_once()
    } else {
        false
    }
}

pub fn has_tasks() -> bool {
    let executor_lock = EXECUTOR.lock();
    if let Some(executor) = executor_lock.as_ref() {
        executor.has_tasks()
    } else {
        false
    }
}

// Dummy waker implementation for basic executor
fn dummy_raw_waker() -> RawWaker {
    fn no_op(_: *const ()) {}
    fn clone(_: *const ()) -> RawWaker {
        dummy_raw_waker()
    }

    let vtable = &RawWakerVTable::new(clone, no_op, no_op, no_op);
    RawWaker::new(core::ptr::null::<()>(), vtable)
}

fn dummy_waker() -> Waker {
    unsafe { Waker::from_raw(dummy_raw_waker()) }
}

// Time-based async timer using actual microseconds
pub struct Timer {
    deadline_us: u64,
}

impl Timer {
    pub fn new(duration_us: u64) -> Self {
        let now = crate::timer::uptime_us();
        Self {
            deadline_us: now.wrapping_add(duration_us),
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        let now = crate::timer::uptime_us();

        // Check if current time has reached or passed the deadline
        if now >= self.deadline_us {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

// Sleep for specified microseconds
pub fn sleep_us(us: u64) -> Timer {
    Timer::new(us)
}

// Sleep for specified milliseconds
pub fn sleep_ms(ms: u64) -> Timer {
    Timer::new(ms * 1_000)
}

// Sleep for specified seconds
pub fn sleep_sec(sec: u64) -> Timer {
    Timer::new(sec * 1_000_000)
}
