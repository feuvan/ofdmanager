//! Native Quick Look support for OFD documents (macOS).
//!
//! This crate is linked into the Quick Look preview and thumbnail appexes as a
//! Rust `staticlib` (`libofd_quicklook.a`) and exposes the frozen C ABI declared
//! in [`include/ofd_quicklook.h`](../../include/ofd_quicklook.h) (interface v1,
//! see `tmp/quicklook-contract.md`):
//!
//! * [`ofd_quicklook_render_preview_pdf`] renders up to `max_pages` pages into a
//!   complete image-based PDF for Finder's spacebar preview.
//! * [`ofd_quicklook_render_thumbnail_png`] renders page 1 to a PNG for Finder
//!   thumbnails.
//! * [`ofd_quicklook_result_free`] releases the buffers returned by both.
//!
//! The safe, testable rendering logic lives in the private `render` module and is
//! re-exported for Rust callers/tests. All entry points are panic-free across the
//! FFI boundary (`catch_unwind`), never return a NULL result pointer except when
//! the allocation itself fails, and are safe to call concurrently from any
//! thread (all state is per-call apart from a lazily initialized, immutable
//! system-font database).
//!
//! Portability: this crate only uses `std`, pure-Rust dependencies, and system
//! font discovery through `fontdb`; it does not link any Apple framework. The
//! renderer reads only the OFD path it is given.

mod render;

pub use render::{render_preview_pdf, render_thumbnail_png, Error};

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::ptr;

/// Result value returned by the render functions (frozen C ABI v1).
///
/// `data`/`error_message` are owned by the result and released by
/// [`ofd_quicklook_result_free`]; callers must not free them themselves.
#[repr(C)]
pub struct OfdQuickLookResult {
    /// 0 on success, otherwise one of the documented status codes.
    pub status: i32,
    /// Owned payload (PDF or PNG bytes); NULL on failure.
    pub data: *mut u8,
    /// Payload length in bytes; 0 when `data` is NULL.
    pub data_len: usize,
    /// Owned UTF-8, NUL-terminated human-readable error; NULL on success.
    pub error_message: *mut c_char,
}

impl OfdQuickLookResult {
    fn success(data: Vec<u8>) -> *mut Self {
        let mut data = data.into_boxed_slice();
        let data_len = data.len();
        let data_ptr = data.as_mut_ptr();
        std::mem::forget(data);
        Box::into_raw(Box::new(Self {
            status: 0,
            data: data_ptr,
            data_len,
            error_message: ptr::null_mut(),
        }))
    }

    fn failure(error: &Error) -> *mut Self {
        let message = c_message(&error.to_string());
        Box::into_raw(Box::new(Self {
            status: error.status(),
            data: ptr::null_mut(),
            data_len: 0,
            error_message: message.into_raw(),
        }))
    }

    fn finish(result: std::thread::Result<Result<Vec<u8>, Error>>) -> *mut Self {
        match result {
            Ok(Ok(data)) => Self::success(data),
            Ok(Err(error)) => Self::failure(&error),
            Err(payload) => Self::failure(&Error::Other(panic_message(payload))),
        }
    }
}

/// Render up to `max_pages` OFD pages into a complete image-based PDF.
///
/// `max_pages == 0` uses 32; `dpi == 0` uses 144 (otherwise clamped to
/// 36..=600); `max_pixels == 0` uses 50,000,000. Returns an `OfdQuickLookResult`
/// whose `data` holds the PDF bytes on success (`status == 0`) or whose
/// `error_message` describes the failure. Free the result with
/// [`ofd_quicklook_result_free`].
///
/// # Safety
///
/// `path_utf8` must be NULL or point to a NUL-terminated C string that stays
/// valid for the duration of the call. The returned pointer must be passed to
/// [`ofd_quicklook_result_free`] exactly once (or be NULL).
#[no_mangle]
pub unsafe extern "C" fn ofd_quicklook_render_preview_pdf(
    path_utf8: *const c_char,
    max_pages: u32,
    dpi: f32,
    max_pixels: u64,
) -> *mut OfdQuickLookResult {
    OfdQuickLookResult::finish(catch_unwind(AssertUnwindSafe(|| {
        let path = c_path(path_utf8)?;
        render_preview_pdf(&path, max_pages, dpi, max_pixels)
    })))
}

/// Render page 1 of an OFD file into an RGBA PNG thumbnail.
///
/// `max_dimension == 0` uses 1024; otherwise it is clamped to 32..=4096 and the
/// page's longest edge is fitted to it while preserving the page aspect ratio.
/// Returns an `OfdQuickLookResult` whose `data` holds the PNG bytes on success
/// (`status == 0`) or whose `error_message` describes the failure. Free the
/// result with [`ofd_quicklook_result_free`].
///
/// # Safety
///
/// `path_utf8` must be NULL or point to a NUL-terminated C string that stays
/// valid for the duration of the call. The returned pointer must be passed to
/// [`ofd_quicklook_result_free`] exactly once (or be NULL).
#[no_mangle]
pub unsafe extern "C" fn ofd_quicklook_render_thumbnail_png(
    path_utf8: *const c_char,
    max_dimension: u32,
) -> *mut OfdQuickLookResult {
    OfdQuickLookResult::finish(catch_unwind(AssertUnwindSafe(|| {
        let path = c_path(path_utf8)?;
        render_thumbnail_png(&path, max_dimension)
    })))
}

/// Release a result returned by [`ofd_quicklook_render_preview_pdf`] or
/// [`ofd_quicklook_render_thumbnail_png`], including its owned data/error
/// buffers. Passing NULL is a no-op.
///
/// # Safety
///
/// `result` must be NULL or a pointer previously returned by one of the render
/// functions in this crate that has not been freed yet.
#[no_mangle]
pub unsafe extern "C" fn ofd_quicklook_result_free(result: *mut OfdQuickLookResult) {
    // A panic must never unwind into Swift/Objective-C; freeing cannot normally
    // fail, but keep the boundary symmetric with the render functions.
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if result.is_null() {
            return;
        }
        let result = Box::from_raw(result);
        if !result.data.is_null() {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                result.data,
                result.data_len,
            )));
        }
        if !result.error_message.is_null() {
            drop(CString::from_raw(result.error_message));
        }
    }));
}

/// Decode the C path argument into an owned Rust path.
///
/// Private and safe on purpose: the raw-pointer dereference is confined to this
/// function, and every caller supplies a pointer from its own FFI parameter.
fn c_path(path_utf8: *const c_char) -> Result<PathBuf, Error> {
    if path_utf8.is_null() {
        return Err(Error::InvalidArgument("path must not be NULL".into()));
    }
    // SAFETY: the FFI contract requires a valid NUL-terminated C string that
    // outlives the call; we only read it here and copy what we need.
    let path = unsafe { CStr::from_ptr(path_utf8) }
        .to_str()
        .map_err(|_| Error::InvalidArgument("path must be valid UTF-8".into()))?;
    if path.is_empty() {
        return Err(Error::InvalidArgument("path must not be empty".into()));
    }
    Ok(Path::new(path).to_path_buf())
}

/// Build an owned, NUL-terminated C string from an error message.
///
/// Interior NULs cannot occur in practice but must not make this fail; they are
/// replaced so a failure always carries a message.
fn c_message(message: &str) -> CString {
    CString::new(message.replace('\0', " "))
        .unwrap_or_else(|_| CString::new("internal error").expect("literal has no NUL"))
}

/// Best-effort text for a caught panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|message| message.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned());
    match detail {
        Some(message) => format!("internal panic: {message}"),
        None => "internal panic".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Swift appex reads `status`/`data`/`data_len`/`error_message` through
    /// the C struct, so the `#[repr(C)]` layout must stay exactly as declared in
    /// `include/ofd_quicklook.h` (compiled in C mode as part of the build check).
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn c_abi_layout_matches_the_header() {
        assert_eq!(std::mem::offset_of!(OfdQuickLookResult, status), 0);
        assert_eq!(std::mem::offset_of!(OfdQuickLookResult, data), 8);
        assert_eq!(std::mem::offset_of!(OfdQuickLookResult, data_len), 16);
        assert_eq!(std::mem::offset_of!(OfdQuickLookResult, error_message), 24);
        assert_eq!(std::mem::size_of::<OfdQuickLookResult>(), 32);
    }

    #[test]
    fn result_buffers_round_trip_through_the_free_function() {
        let result = OfdQuickLookResult::success(b"hello".to_vec());
        assert!(!result.is_null());
        // SAFETY: freshly created result, not yet freed.
        unsafe {
            assert_eq!((*result).status, 0);
            assert_eq!((*result).data_len, 5);
            assert!((*result).error_message.is_null());
            assert_eq!(
                std::slice::from_raw_parts((*result).data, (*result).data_len),
                b"hello"
            );
            ofd_quicklook_result_free(result);
        }

        let error = Error::Render("boom".into());
        let result = OfdQuickLookResult::failure(&error);
        // SAFETY: freshly created result, not yet freed.
        unsafe {
            assert_eq!((*result).status, 5);
            assert!((*result).data.is_null());
            let message = CStr::from_ptr((*result).error_message);
            assert_eq!(message.to_str().unwrap(), "boom");
            ofd_quicklook_result_free(result);
        }
    }
}
