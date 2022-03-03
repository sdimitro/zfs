use crate::get_tunable;
use backtrace::Backtrace;
use lazy_static::lazy_static;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::{self, Display};
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

pub struct TrackingAllocator;
thread_local! {
    static ALLOCD_SINCE_BACKTRACE: Cell<u64> = Default::default();
    static ALLOC_TAG: Cell<Option<AllocTag>> = Default::default();
}

lazy_static! {
    // PerAllocInfo contains the `&'static Key` that's stored in this HashMap.
    static ref ALLOCS: Mutex<HashMap<&'static Key, PerBacktraceInfo>> = Default::default();
}

// See TrackingAllocator::setup()
static BACKTRACE_ENABLED: AtomicBool = AtomicBool::new(false);
static BACKTRACE_INTERVAL: AtomicU64 = AtomicU64::new(7 * 997); // bytes allocd
static ALLOC_TAG_OTHER: AtomicBool = AtomicBool::new(false);
static ALLOC_TAG_HIGH_FREQ: AtomicBool = AtomicBool::new(false);

// Number of stack frames to use as the key.  If the first 10 frames are the
// same, we assume the entire backtrace is the same, and count them together.
// Note that the entire backtrace is recorded in the value (PerBacktraceInfo).
const BACKTRACE_FRAMES: usize = 10;

// The type that callers use to tag allocations; typically a string literal.
type AllocTag = &'static str;

#[derive(PartialEq, Eq, Hash)]
enum Key {
    Backtrace([usize; BACKTRACE_FRAMES]),
    Tag(AllocTag),
}

impl Key {
    fn from_backtrace(bt: &Backtrace) -> Self {
        let mut array = [0usize; BACKTRACE_FRAMES];

        for (i, frame) in bt.frames().iter().enumerate().take(BACKTRACE_FRAMES) {
            array[i] = frame.ip() as usize;
        }

        Key::Backtrace(array)
    }
}

/// Call the function.  If any memory allocations are made while the function is
/// running, mark them with the given tag, typically a "string literal".  All
/// allocations with the same tag will be reported together by
/// TrackingAllocator::format().
pub fn with_alloctag<F, R>(tag: AllocTag, f: F) -> R
where
    F: FnOnce() -> R,
{
    let old_tag = ALLOC_TAG.with(|a| a.replace(Some(tag)));
    let result = f();
    ALLOC_TAG.with(|a| a.set(old_tag));
    result
}

// For high frequency callers - tracking disabled by default.
pub fn with_alloctag_hf<F, R>(tag: AllocTag, f: F) -> R
where
    F: FnOnce() -> R,
{
    if ALLOC_TAG_HIGH_FREQ.load(Ordering::Relaxed) {
        let old_tag = ALLOC_TAG.with(|a| a.replace(Some(tag)));
        let result = f();
        ALLOC_TAG.with(|a| a.set(old_tag));
        result
    } else {
        f()
    }
}

#[derive(Default, Clone)]
struct PerBacktraceInfo {
    bt: Backtrace,
    allocs: u64,
    frees: u64,
    capacity: u64, // i.e. bytes currently allocated
}

impl Display for PerBacktraceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "did {} allocs and {} frees, {}MB ({} items) currently allocd ({} bytes/item)",
            self.allocs,
            self.frees,
            self.capacity / 1024 / 1024,
            self.allocs - self.frees,
            self.capacity
                .checked_div(self.allocs - self.frees)
                .unwrap_or_default(),
        )
    }
}

#[derive(Default)]
struct PerAllocInfo {
    // We need to keep this struct as small as possible, since it's tacked on to
    // every allocation.  Rather than storing a Key (which is at least
    // BACKTRACE_FAMES words long), we store a reference to a Key, which is one
    // word.  The referred-to key is stored in the ALLOCS HashMap and can never
    // be freed.
    key: Option<&'static Key>,
}

impl PerAllocInfo {
    fn new(layout: Layout) -> Self {
        nonrecursive(|| {
            let key = match ALLOC_TAG.with(|a| a.get()) {
                Some(str) => Key::Tag(str),
                None => {
                    if BACKTRACE_ENABLED.load(Ordering::Relaxed) {
                        let gather_backtrace = ALLOCD_SINCE_BACKTRACE.with(|asb_cell| {
                            let allocd_since_backtrace = asb_cell.get() + layout.size() as u64;
                            let backtrace_interval = BACKTRACE_INTERVAL.load(Ordering::Relaxed);
                            asb_cell.set(allocd_since_backtrace % backtrace_interval);
                            allocd_since_backtrace > backtrace_interval
                        });
                        if gather_backtrace {
                            Key::from_backtrace(&Backtrace::new_unresolved())
                        } else {
                            // Computing the backtrace is too expensive; don't track this allocation.
                            return Default::default();
                        }
                    } else if ALLOC_TAG_OTHER.load(Ordering::Relaxed) {
                        Key::Tag("UNTAGGED")
                    } else {
                        // Tracking every allocation is too expensive (lock contention on ALLOCS).
                        return Default::default();
                    }
                }
            };

            let mut allocs = ALLOCS.lock().unwrap();
            let key_ref = match allocs.get_key_value(&key).map(|(k, _)| *k) {
                Some(key_ref) => key_ref,
                None => {
                    let key_ref = Box::leak(Box::new(key)) as &Key;
                    allocs.insert(key_ref, Default::default());
                    key_ref
                }
            };
            let e = allocs.get_mut(key_ref).unwrap();
            e.allocs += 1;
            e.capacity += layout.size() as u64;
            PerAllocInfo { key: Some(key_ref) }
        })
        .unwrap_or_default()
    }

    fn decrement(&self, layout: Layout) {
        // Note: allocating memory could result in infinite recursion, and must be avoided.
        if let Some(key) = self.key {
            let mut h = ALLOCS.lock().unwrap();

            let e = h.entry(key).or_default();
            e.frees += 1;
            e.capacity -= layout.size() as u64;
        }
    }
}

/// Each allocation has a PerAllocInfo appended to it.  This calculates the
/// Layout of the "real" allocation which includes the PerAllocInfo.  returns
/// (new_layout, offset_of_PerAllocInfo)
fn new_layout(layout: Layout) -> (Layout, isize) {
    static EMPTY_INFO: PerAllocInfo = PerAllocInfo { key: None };
    let (new_layout, offset) = layout.extend(Layout::for_value(&EMPTY_INFO)).unwrap();
    (new_layout, offset.try_into().unwrap())
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let info = PerAllocInfo::new(layout);
        let (new_layout, info_offset) = new_layout(layout);
        let ptr = System.alloc(new_layout);
        if !ptr.is_null() {
            let info_ptr = ptr.offset(info_offset) as *mut PerAllocInfo;
            *info_ptr = info;
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Note: allocating memory here could result in infinite recursion, and must be avoided.
        let (new_layout, info_offset) = new_layout(layout);
        let info_ptr = ptr.offset(info_offset) as *const PerAllocInfo;
        (*info_ptr).decrement(layout);

        System.dealloc(ptr, new_layout)
    }
}

impl TrackingAllocator {
    /// Get tunables.  This has to be done explicitly, after the tunables are
    /// loaded.  If we used lazy_static! to get the tunables, we'd try to get
    /// them before they were loaded.
    pub fn setup() {
        BACKTRACE_ENABLED.store(get_tunable("alloc_backtrace", false), Ordering::Relaxed);
        BACKTRACE_INTERVAL.store(
            get_tunable("alloc_backtrace_interval", 7 * 997),
            Ordering::Relaxed,
        );
        ALLOC_TAG_OTHER.store(get_tunable("alloc_tag_other", false), Ordering::Relaxed);
        ALLOC_TAG_HIGH_FREQ.store(get_tunable("alloc_tag_high_freq", false), Ordering::Relaxed);
    }

    pub fn format(min_allocs: u64, min_bytes: u64) -> DelayedFormat {
        DelayedFormat {
            min_allocs,
            min_bytes,
        }
    }
}

pub struct DelayedFormat {
    min_allocs: u64,
    min_bytes: u64,
}

impl Display for DelayedFormat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // nonrecursive() disables allocation tracking, so that we won't try to
        // recursively acquire the ALLOCS lock and deadlock.
        let (total, mut vec) = nonrecursive(|| {
            // Note: we can't use `f` while holding the ALLOCS lock, because it
            // may need to free something that was allocated outside of the
            // `nonrecursive()` and is therefore tracked and will need the
            // ALLOCS lock. (e.g. to realloc a buffer that write!(f) will append
            // to)
            let allocs = ALLOCS.lock().unwrap();
            let total = allocs
                .values()
                .fold(PerBacktraceInfo::default(), |mut a, b| {
                    a.allocs += b.allocs;
                    a.frees += b.frees;
                    a.capacity += b.capacity;
                    a
                });
            let vec = allocs
                .iter()
                .filter(|(key, info)| {
                    matches!(key, Key::Tag(_))
                        || info.allocs > self.min_allocs
                        || info.capacity >= self.min_bytes
                })
                .map(|(key, info)| (*key, info.clone()))
                .collect::<Vec<_>>();
            (total, vec)
        })
        .unwrap(); // we shouldn't be in the allocator when calling this

        vec.sort_by_key(|(_, info)| info.capacity);
        for (key, info) in vec {
            match key {
                Key::Backtrace(_) => {
                    writeln!(f, "{}", info)?;
                    writeln!(f, "{:?}", info.bt)?;
                }
                Key::Tag(tag) => writeln!(f, "{} from tag {}", info, tag)?,
            }
        }
        writeln!(f, "TOTAL TRACKED: {}", total)?;
        Ok(())
    }
}

/// Invoke function and return Some(f()), unless this is called recursively, in
/// which case do not invoke function, and return None.
fn nonrecursive<F, R>(f: F) -> Option<R>
where
    F: FnOnce() -> R,
{
    thread_local! {
        static EXECUTING: Cell<bool> = Default::default();
    }
    if EXECUTING.with(|a| a.replace(true)) {
        // Already executing nonrecursive(); return None without executing f()
        None
    } else {
        let r = f();
        EXECUTING.with(|a| a.set(false));
        Some(r)
    }
}
