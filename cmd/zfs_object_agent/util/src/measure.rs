use core::fmt;
use std::fmt::Display;
use std::future::Future;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::Once;

use tokio::task::JoinHandle;

use crate::lazy_static_ptr;

lazy_static_ptr! {
    static ref MEASUREMENTS: Mutex<Vec<&'static Measurement>> = Default::default();
}

pub struct Measurement {
    name: &'static str,
    initializer: Once,
    count: AtomicU64,
    inflight: AtomicU64,
}

impl Measurement {
    /// Construct a new Measurement.  Typically used via the `measure!` macro.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            initializer: Once::new(),
            count: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
        }
    }

    /// Add this to the global registry.  This is idempotent - it uses `std::Sync::Once` to
    /// cheaply ensure that this instance is added only once.  Typically used via the `measure!`
    /// macro.
    pub fn register(&'static self) {
        self.initializer.call_once(|| {
            MEASUREMENTS.lock().unwrap().push(self);
        });
    }

    /// Measure the execution of the provided future (when the implicitly-returned Future is
    /// awaited).
    pub async fn fut<F, R>(&self, future: F) -> R
    where
        F: Future<Output = R>,
    {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.inflight.fetch_add(1, Ordering::Relaxed);
        let result = future.await;
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        result
    }

    /// Measure the execution of the provided closure.
    pub fn func<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.inflight.fetch_add(1, Ordering::Relaxed);
        let result = f();
        self.inflight.fetch_sub(1, Ordering::Relaxed);
        result
    }

    /// `tokio::spawn()` a new future, and measure its execution.  Within zfs_object_agent, this
    /// should generally be used instead of plain `tokio::spawn()`.
    pub fn spawn<T>(&'static self, future: T) -> JoinHandle<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        tokio::spawn(self.fut(future))
    }
}

impl Display for Measurement {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}: {} calls, {} inflight",
            self.name,
            self.count.load(Ordering::Relaxed),
            self.inflight.load(Ordering::Relaxed)
        )?;
        Ok(())
    }
}

pub struct DelayedFormat;

/// Return a struct whose Display will print the global registry of measurements.
pub fn dump() -> DelayedFormat {
    DelayedFormat
}

impl Display for DelayedFormat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut measurements = MEASUREMENTS.lock().unwrap().clone();
        measurements.sort_by_key(|a| a.inflight.load(Ordering::Relaxed));

        for measurement in measurements {
            writeln!(f, "{}", measurement)?;
        }
        Ok(())
    }
}

/// Create and return a new `&'static Measurement`, and add it to the global registry which is
/// printed by dump().  The instance will be identified by the call site (file:line:col), and
/// optionally a string literal argument.
/// ```
/// measure!().fut(async move {...}).await;
/// measure!("identifying tag").func(|| {...}).await;
/// ```
#[macro_export]
macro_rules! measure {
    ($l:literal) => {{
        static MEASUREMENT: $crate::measure::Measurement = $crate::measure::Measurement::new(
            concat!($l, " (", file!(), ":", line!(), ":", column!(), ")"),
        );
        MEASUREMENT.register();
        &MEASUREMENT
    }};
    () => {
        $crate::measure!("")
    };
}
