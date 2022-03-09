use std::collections::HashSet;
use std::ops::Deref;
use std::ops::DerefMut;
pub use std::sync::atomic::AtomicPtr;
pub use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use derivative::Derivative;
pub use lazy_static::lazy_static;
pub use paste::paste;

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

#[derive(Derivative)]
#[derivative(Hash(bound = ""))]
#[derivative(PartialEq(bound = ""))]
#[derivative(Eq(bound = ""))]
/// This wrapping struct exists so that we can mark the raw pointer as Send+Sync (i.e. safe to
/// use from different threads).
struct DebugPointer<T>(*const T);
unsafe impl<T> Send for DebugPointer<T> {}
unsafe impl<T> Sync for DebugPointer<T> {}

impl<T> DebugPointer<T> {
    fn new(guard: &DebugPointerGuard<T>) -> Self {
        Self(&*guard.value)
    }
}

/// A RAII guard that deref's to the stored value.  When dropped, its pointer will be removed
/// from the DebugPointerSet that it was created from.
pub struct DebugPointerGuard<T> {
    value: Box<T>,
    set: DebugPointerSet<T>,
}

impl<T> Deref for DebugPointerGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &*self.value
    }
}

impl<T> DerefMut for DebugPointerGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.value
    }
}

impl<T> Drop for DebugPointerGuard<T> {
    fn drop(&mut self) {
        let removed = self
            .set
            .set
            .lock()
            .unwrap()
            .remove(&DebugPointer::new(self));
        assert!(removed);
    }
}

#[derive(Derivative)]
#[derivative(Clone(bound = ""))]
#[derivative(Default(bound = ""))]
/// This is a set of structs, which we will store pointers to so that we can find them in the
/// debugger.  Note that the stored type T is unconstrained (e.g. it need not be Hash), because
/// we are storing (but not dereferencing) a pointer to it.  Typical use is combined with
/// `lazy_static_ptr!`:
/// ```
/// fn func(thing: Thing) {
///     lazy_static_ptr! {
///         static ref THINGS: DebugPointerSet<Thing> = Default::default();
///     }
///     let mut thing = THINGS.insert(thing);
///     // use `thing` as usual, it deref's to the passed in Thing
///     thing.method();
/// }
/// ```
/// Note that `lazy_static_ptr!` doesn't work well inside `async`
/// functions/methods/closures, because it's hard to name the variable in the
/// debugger (it has {braces} in its name).  The workaround is to either create
/// the lazy_static_ptr! at the file level (not inside a function), or to
/// desugar the `async fn` to a regular `fn` that returns a `Future`.
pub struct DebugPointerSet<T> {
    set: Arc<Mutex<HashSet<DebugPointer<T>>>>,
}

impl<T> DebugPointerSet<T> {
    pub fn new() -> Self {
        Default::default()
    }

    /// Insert a new object to the DebugPointerSet.  The debugger can be used to find a pointer
    /// to the object.  The object is moved into the returned DebugPointerGuard, which holds the
    /// object in a Box, so that its location in memory doesn't change.  The DebugPointerGuard
    /// can be dereferenced to the contained object. Note that the pointer tracks the location of
    /// the DebugPointerGuard's contents, even if the object is moved out of the Guard with
    /// `mem::replace()` or `mem::take()`.
    pub fn insert(&self, value: T) -> DebugPointerGuard<T> {
        let guard = DebugPointerGuard {
            value: Box::new(value),
            set: self.clone(),
        };
        let inserted = self.set.lock().unwrap().insert(DebugPointer::new(&guard));
        assert!(inserted);
        guard
    }
}
