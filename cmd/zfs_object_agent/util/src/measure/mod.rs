pub mod lock;
pub mod lock_non_send;

use core::fmt;
use std::fmt::Display;
use std::future::Future;
use std::mem::size_of_val;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::sync::Once;
use std::time::Instant;

use futures::FutureExt;
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
    fut_size: AtomicUsize,
    nanos: AtomicU64,
}

impl Measurement {
    /// Construct a new Measurement.  Typically used via the `measure!` macro.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            initializer: Once::new(),
            count: AtomicU64::new(0),
            inflight: AtomicU64::new(0),
            fut_size: AtomicUsize::new(0),
            nanos: AtomicU64::new(0),
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

    fn begin(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.inflight.fetch_add(1, Ordering::Relaxed);
    }

    fn end(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    fn begin_timed(&self) -> Instant {
        self.begin();
        Instant::now()
    }

    fn end_timed(&self, begin: Instant) {
        self.end();
        #[allow(clippy::cast_possible_truncation)]
        let elapsed = begin.elapsed().as_nanos() as u64;
        self.nanos.fetch_add(elapsed, Ordering::Relaxed);
    }

    /// Wrap the provided future in one that will measure its execution.
    // Lifetime annotations say that self must live longer than the `future` argument.  This is
    // typically satisfied by `&'static self`, i.e. the static Measurement created by `measure!()`.
    pub fn fut<'a, 'b, R>(
        &'a self,
        future: impl Future<Output = R> + 'b,
    ) -> impl Future<Output = R> + 'b
    where
        'a: 'b,
        R: 'b,
    {
        if self.fut_size.load(Ordering::Relaxed) == 0 {
            // Multiple threads may race to set this, but they will all store the same value, so
            // it isn't worth using a Once to guarantee that it doesn't get stored multiple
            // times.
            self.fut_size.store(size_of_val(&future), Ordering::Relaxed);
        }
        self.begin();
        // We don't use an async function or closure because it doubles the size of the future.
        future.inspect(|_| self.end())
    }

    pub fn fut_timed<'a, 'b, R>(
        &'a self,
        future: impl Future<Output = R> + 'b,
    ) -> impl Future<Output = R> + 'b
    where
        'a: 'b,
        R: 'b,
    {
        let begin = Instant::now();
        self.fut(future).inspect(move |_| {
            #[allow(clippy::cast_possible_truncation)]
            let elapsed = begin.elapsed().as_nanos() as u64;
            self.nanos.fetch_add(elapsed, Ordering::Relaxed);
        })
    }

    /// Measure the execution of the provided closure.
    pub fn func<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        self.begin();
        let result = f();
        self.end();
        result
    }

    /// Measure the execution of the provided closure.
    pub fn func_timed<F, R>(&self, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let begin = self.begin_timed();
        let result = self.func(f);
        self.end_timed(begin);
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

    pub fn hit(&self) {
        self.count.fetch_add(1, Ordering::Relaxed);
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
        let nanos = self.nanos.load(Ordering::Relaxed);
        if nanos != 0 {
            write!(f, ", {}ms total", nanos / 1_000_000)?;
        }
        let fut_size = self.fut_size.load(Ordering::Relaxed);
        if fut_size != 0 {
            write!(f, ", fut_size {}B", fut_size)?;
        }
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
/// ```ignore
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

#[cfg(test)]
mod test {
    use std::mem::size_of_val;

    use more_asserts::*;

    #[test]
    fn nonexponential() {
        let f = futures::future::ready(1u64);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        let f = measure!().fut(f);
        assert_le!(size_of_val(&f), 1024);
    }
}
