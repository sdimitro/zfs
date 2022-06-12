use std::ops::Deref;
use std::ops::DerefMut;
use std::time::Instant;

use super::Measurement;

pub struct MeasuredMutexGuard<'a, T> {
    inner: tokio::sync::MutexGuard<'a, T>,
    begin: Instant,
    hold: &'static Measurement,
}

impl<'a, T> Drop for MeasuredMutexGuard<'a, T> {
    fn drop(&mut self) {
        self.hold.end_timed(self.begin);
    }
}

impl<'a, T> Deref for MeasuredMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T> DerefMut for MeasuredMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub async fn lock<'a, T>(
    mutex: &'a tokio::sync::Mutex<T>,
    acquire: &'static Measurement,
    hold: &'static Measurement,
) -> MeasuredMutexGuard<'a, T> {
    MeasuredMutexGuard {
        inner: acquire.fut_timed(mutex.lock()).await,
        hold,
        begin: hold.begin_timed(),
    }
}

/// This locks the mutex like `Mutex::lock()`, but measures the time spent waiting for the lock,
/// and the time spent holding the lock.
#[macro_export]
macro_rules! lock_measured {
    ($lock:expr, $tag:literal) => {{
        static ACQUIRE: $crate::measure::Measurement = $crate::measure::Measurement::new(concat!(
            "acquire lock ",
            $tag,
            " (",
            file!(),
            ":",
            line!(),
            ":",
            column!(),
            ")"
        ));
        ACQUIRE.register();
        static HOLD: $crate::measure::Measurement = $crate::measure::Measurement::new(concat!(
            "hold lock ",
            $tag,
            " (",
            file!(),
            ":",
            line!(),
            ":",
            column!(),
            ")"
        ));
        HOLD.register();

        $crate::measure::lock::lock($lock, &ACQUIRE, &HOLD)
    }};
    ($lock:expr) => {
        $crate::lock_measured!($lock, "")
    };
}
