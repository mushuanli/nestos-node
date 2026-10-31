//! Preserve capability paths so SQLite does not resolve /proc/self/fd back to a mutable absolute path.
use crate::core::error::Error;
use rusqlite::ffi;
use std::{
    ffi::{c_char, c_int, CStr},
    sync::OnceLock,
};

pub(super) fn name() -> Result<&'static str, Error> {
    static REGISTERED: OnceLock<c_int> = OnceLock::new();
    let result = *REGISTERED.get_or_init(|| unsafe {
        let base = ffi::sqlite3_vfs_find(std::ptr::null());
        if base.is_null() {
            return ffi::SQLITE_CANTOPEN;
        }
        // All OS operations retain the stock Unix implementation (including O_NOFOLLOW).
        // Only path canonicalization changes; the pinned parent descriptor remains live.
        let mut vfs = Box::new(std::ptr::read(base));
        vfs.zName = c"nestos-capability".as_ptr();
        vfs.pNext = std::ptr::null_mut();
        vfs.xFullPathname = Some(full_path);
        let pointer = Box::into_raw(vfs);
        let result = ffi::sqlite3_vfs_register(pointer, 0);
        if result != ffi::SQLITE_OK {
            drop(Box::from_raw(pointer));
        }
        result
    });
    if result != ffi::SQLITE_OK {
        return Err(Error::internal());
    }
    Ok("nestos-capability")
}
unsafe extern "C" fn full_path(
    _vfs: *mut ffi::sqlite3_vfs,
    input: *const c_char,
    size: c_int,
    output: *mut c_char,
) -> c_int {
    let path = CStr::from_ptr(input).to_bytes_with_nul();
    if !path.starts_with(b"/proc/self/fd/") || size <= 0 || path.len() > size as usize {
        return ffi::SQLITE_CANTOPEN;
    }
    std::ptr::copy_nonoverlapping(path.as_ptr().cast::<c_char>(), output, path.len());
    ffi::SQLITE_OK
}
