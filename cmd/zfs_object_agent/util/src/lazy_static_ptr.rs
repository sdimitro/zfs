pub use lazy_static::lazy_static;
pub use paste::paste;
pub use std::sync::atomic::{AtomicPtr, Ordering};

/// This macro is similar to `lazy_static!`, but for each static created, it creates an
/// additional static variable with the `_PTR` suffix, which is an `AtomicPtr` to the contents of
/// the lazy_static variable.  This can be used by the debugger to find the contents of the
/// lazy_static, which are otherwise buried inside a closure, which makes it hard to name from
/// the global context of the debugger.
///
/// For example:
/// ```
/// lazy_static_ptr! {
///    pub static ref STRINGS: Mutex<Vec<String>> = Default::default();
/// }
/// ```
/// This will declare a `pub static` variable named `STRINGS` which will deref to a
/// `Box<Mutex<Vec<String>>>`, similar to lazy_static!.  Additionally, it will declare
/// ```
/// static STRINGS_PTR: AtomicPtr<Mutex<Vec<String>>> = ...;
/// ```
/// When `STRINGS` is initialized (on first dereference, or the .initialize() method),
/// `STRINGS_PTR` will be set to the location of the value returned by the initializer.
///
/// This macro uses `lazy_static!` internally.  In order to compute the value's location in the
/// lazy_static initializer, it is allocated on the heap (in a `Box`), so `STRINGS` actually
/// dereferences to a `Box<Mutex<Vec<String>>>`, which itself deref's to a `Mutex<Vec<String>>`.
/// So the contents can be referred to by double-dereferenceing the global, `**STRINGS`.  This
/// happens automatically for method calls, e.g. `STRINGS.lock()`
#[macro_export]
macro_rules! lazy_static_ptr {
    // "pub" declaration
    (pub static ref $N:ident : $T:ty = $e:expr; $($t:tt)*) => {
        $crate::lazy_static_ptr!{ @IMPL (pub) $N: $T = $e; }
        $crate::lazy_static_ptr!($($t)*);
    };

    // non-"pub" declaration
    (static ref $N:ident : $T:ty = $e:expr; $($t:tt)*) => {
        $crate::lazy_static_ptr!{ @IMPL () $N: $T = $e; }
        $crate::lazy_static_ptr!($($t)*);
    };

    // internal implementation
    (@IMPL ($($vis:tt)*) $N:ident : $T:ty = $e:expr; $($t:tt)*) => {
        $crate::lazy_static_ptr::paste! {
            static [<$N _PTR>]: $crate::lazy_static_ptr::AtomicPtr<$T> =
                $crate::lazy_static_ptr::AtomicPtr::new(::std::ptr::null_mut());
        }
        $crate::lazy_static_ptr::lazy_static! {
            $($vis)* static ref $N: Box<$T> = {
                let mut this: Box<$T> = Box::new($e);
                $crate::lazy_static_ptr::paste! {
                    [<$N _PTR>].store(&mut *this, $crate::lazy_static_ptr::Ordering::Relaxed);
                }
                this
            };
        }
    };

    // empty trailing tokens
    () => ()
}
