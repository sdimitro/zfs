use std::marker::PhantomData;
use std::ops::Deref;
use std::ops::DerefMut;

use super::lock::MeasuredMutexGuard;
use super::Measurement;

pub struct NonSendMeasuredMutexGuard<'a, T> {
    inner: MeasuredMutexGuard<'a, T>,
    _marker: PhantomData<*const ()>,
}

impl<'a, T> Deref for NonSendMeasuredMutexGuard<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'a, T> DerefMut for NonSendMeasuredMutexGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub async fn lock_non_send<'a, T>(
    mutex: &'a tokio::sync::Mutex<T>,
    acquire: &'static Measurement,
    hold: &'static Measurement,
) -> NonSendMeasuredMutexGuard<'a, T> {
    NonSendMeasuredMutexGuard {
        inner: super::lock::lock(mutex, acquire, hold).await,
        _marker: PhantomData,
    }
}

/// This locks the mutex like `Mutex::lock()`, but measures it (like `lock_measured!`), and
/// returns a new kind of guard which can not be sent between threads.  This is useful if you
/// want to ensure that .await is not used while the mutex is locked by some callers, but .await
/// can be used from other callers (that use `lock_measured!` or `tokio::sync::Mutex::lock()`
/// directly).
#[macro_export]
macro_rules! lock_non_send_measured {
    ($lock:expr, $tag:literal) => {{
        static ACQUIRE: $crate::measure::Measurement = $crate::measure::Measurement::new(concat!(
            "acquire lock non send",
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
            "hold lock non send",
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

        $crate::measure::lock_non_send::lock_non_send($lock, &ACQUIRE, &HOLD)
    }};
    ($lock:expr) => {
        $crate::lock_non_send_measured!($lock, "")
    };
}
