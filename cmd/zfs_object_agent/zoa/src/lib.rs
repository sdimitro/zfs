use std::ffi::CStr;
use std::ffi::OsStr;
use std::os::raw::c_char;
use std::os::unix::prelude::OsStrExt;
use std::path::Path;

use foreign_types::ForeignType;
use libc::c_void;
use nix::errno::Errno;
use nvpair::NvList;
use nvpair::NvListRef;
use zettacache::base_types::PoolGuid;
use zettacache::CacheOpenMode;
use zettaobject::base_types::Txg;
use zettaobject::debug::DebugHandle;

#[allow(non_camel_case_types)]
pub type zoa_handle_t = c_void;

/// # Safety
/// The c_char pointers must be to actual C strings. handle must be a valid pointer to a void *, or
/// NULL.
#[no_mangle]
pub unsafe extern "C" fn libzoa_init(
    socket_dir_ptr: *const c_char,
    log_file_ptr: *const c_char,
    cache_path_ptr: *const c_char, // XXX change to take a list of paths
    handle: *mut *mut zoa_handle_t,
) -> i32 {
    let socket_dir = Path::new(OsStr::from_bytes(CStr::from_ptr(socket_dir_ptr).to_bytes()));
    let log_file = Path::new(OsStr::from_bytes(CStr::from_ptr(log_file_ptr).to_bytes()));

    let verbosity = 2;
    util::setup_logging(verbosity, Some(log_file), None, false);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zoa")
        .build()
        .unwrap();

    if !handle.is_null() {
        *handle = Box::into_raw(Box::new(DebugHandle::new(runtime.handle().clone()))).cast();
    }

    if cache_path_ptr.is_null() {
        if zettaobject::init::start(socket_dir, CacheOpenMode::None, false, runtime).is_err() {
            return -1;
        }
    } else {
        let cache_path = Path::new(OsStr::from_bytes(CStr::from_ptr(cache_path_ptr).to_bytes()));
        if zettaobject::init::start(
            socket_dir,
            CacheOpenMode::DeviceList(vec![cache_path.to_owned()]),
            false,
            runtime,
        )
        .is_err()
        {
            return -1;
        }
    }
    0
}

unsafe fn set_out_nvl(out: *mut *mut nvpair_sys::nvlist_t, result: Result<NvList, Errno>) -> i32 {
    match result {
        Ok(result) => {
            *out = result.into_ptr();
            0
        }
        Err(i) => i as i32,
    }
}

/// # Safety
/// In order to use this function safely, handle must be a pointer that was previously returned by
/// libzoa_init().
#[no_mangle]
pub unsafe extern "C" fn libzoa_open_pool(
    raw_handle: *mut zoa_handle_t,
    guid: u64,
    raw_nvl: *const nvpair_sys::nvlist_t,
) -> i32 {
    let handle = raw_handle.cast::<DebugHandle>().as_mut().unwrap();
    let nvl = NvListRef::from_ptr(raw_nvl);
    handle
        .open_pool(PoolGuid(guid), nvl)
        .map_or_else(|errno| errno as i32, |_| 0)
}

/// # Safety
/// In order to use this function safely:
/// * out must be a valid pointer to a not-necessarily valid pointer to an nvlist_t.
/// * handle must be a pointer that was previously returned by libzoa_init().
#[no_mangle]
pub unsafe extern "C" fn libzoa_get_pool_phys(
    raw_handle: *mut zoa_handle_t,
    guid: u64,
    out: *mut *mut nvpair_sys::nvlist_t,
) -> i32 {
    let handle = raw_handle.cast::<DebugHandle>().as_mut().unwrap();
    let res = handle.get_pool_phys(PoolGuid(guid));
    set_out_nvl(out, res)
}

/// # Safety
/// In order to use this function safely:
/// * out must be a valid pointer to a not-necessarily valid pointer to an nvlist_t.
/// * handle must be a pointer that was previously returned by libzoa_init().
#[no_mangle]
pub unsafe extern "C" fn libzoa_get_uberblock_phys(
    raw_handle: *mut zoa_handle_t,
    guid: u64,
    txg: u64,
    out: *mut *mut nvpair_sys::nvlist_t,
) -> i32 {
    let handle = raw_handle.cast::<DebugHandle>().as_mut().unwrap();
    let res = handle.get_uberblock_phys(PoolGuid(guid), Txg(txg));
    set_out_nvl(out, res)
}
