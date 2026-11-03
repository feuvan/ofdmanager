//! Tests for the frozen C ABI in `include/ofd_quicklook.h`, called exactly as
//! the Swift appexes will call it: raw pointers in, owned result out, freed via
//! `ofd_quicklook_result_free`.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::ptr;

use ofd_quicklook::{
    ofd_quicklook_render_preview_pdf, ofd_quicklook_render_thumbnail_png,
    ofd_quicklook_result_free, OfdQuickLookResult,
};

const PDF_MAGIC: &[u8] = b"%PDF";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn c_path(path: &Path) -> CString {
    CString::new(path.to_string_lossy().as_bytes()).expect("fixture path has no NUL")
}

/// Owns one FFI result: copies the exposed fields out, validates the invariants
/// every result must uphold, then frees it with the C API.
fn take_result(result: *mut OfdQuickLookResult) -> (i32, Vec<u8>, Option<String>) {
    assert!(!result.is_null(), "render functions must not return NULL");
    // SAFETY: the pointer comes from ofd_quicklook_render_* and is still owned.
    let status = unsafe { (*result).status };
    // SAFETY: same result, not yet freed.
    let data = unsafe {
        if (*result).data.is_null() {
            assert_eq!((*result).data_len, 0, "NULL data must have zero length");
            Vec::new()
        } else {
            std::slice::from_raw_parts((*result).data, (*result).data_len).to_vec()
        }
    };
    // SAFETY: same result, not yet freed.
    let error_message = unsafe {
        if (*result).error_message.is_null() {
            None
        } else {
            Some(
                CStr::from_ptr((*result).error_message)
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    if status == 0 {
        assert!(!data.is_empty(), "successful result must carry data");
        assert!(error_message.is_none(), "success must not carry an error");
    } else {
        assert!(data.is_empty(), "failure must not carry data");
        let message = error_message.as_deref().unwrap_or("");
        assert!(!message.is_empty(), "failure must carry an error message");
    }
    // SAFETY: the pointer is a valid, unfreed result; this is the only free.
    unsafe { ofd_quicklook_result_free(result) };
    (status, data, error_message)
}

#[test]
fn preview_pdf_over_the_c_abi() {
    let path = c_path(&fixture("helloworld.ofd"));
    // 72 DPI keeps this test fast while still exercising the full render path.
    // SAFETY: `path` is a valid C string for the duration of the call.
    let result = unsafe { ofd_quicklook_render_preview_pdf(path.as_ptr(), 0, 72.0, 0) };
    let (status, data, _) = take_result(result);
    assert_eq!(status, 0);
    assert!(data.starts_with(PDF_MAGIC));
}

#[test]
fn thumbnail_png_over_the_c_abi() {
    let path = c_path(&fixture("helloworld.ofd"));
    // SAFETY: `path` is a valid C string for the duration of the call.
    let result = unsafe { ofd_quicklook_render_thumbnail_png(path.as_ptr(), 64) };
    let (status, data, _) = take_result(result);
    assert_eq!(status, 0);
    assert!(data.starts_with(PNG_MAGIC));
}

#[test]
fn null_path_is_status_1() {
    // SAFETY: NULL is explicitly handled by the FFI layer.
    let result = unsafe { ofd_quicklook_render_preview_pdf(ptr::null(), 0, 0.0, 0) };
    let (status, _, error_message) = take_result(result);
    assert_eq!(status, 1);
    assert!(error_message.unwrap().contains("NULL"));
}

#[test]
fn non_utf8_path_is_status_1() {
    let invalid = CString::new(vec![0x2f, 0x74, 0x6d, 0x70, 0x2f, 0xff]).expect("no NUL");
    // SAFETY: `invalid` is a valid NUL-terminated C string.
    let result = unsafe { ofd_quicklook_render_thumbnail_png(invalid.as_ptr(), 0) };
    let (status, _, error_message) = take_result(result);
    assert_eq!(status, 1);
    assert!(error_message.unwrap().contains("UTF-8"));
}

#[test]
fn empty_path_is_status_1() {
    let empty = CString::new("").expect("empty C string");
    // SAFETY: `empty` is a valid NUL-terminated C string.
    let result = unsafe { ofd_quicklook_render_thumbnail_png(empty.as_ptr(), 0) };
    let (status, _, _) = take_result(result);
    assert_eq!(status, 1);
}

#[test]
fn missing_file_is_status_2_with_a_message() {
    let path = c_path(&fixture("definitely-not-here.ofd"));
    // SAFETY: `path` is a valid C string for the duration of the call.
    let result = unsafe { ofd_quicklook_render_preview_pdf(path.as_ptr(), 0, 0.0, 0) };
    let (status, _, error_message) = take_result(result);
    assert_eq!(status, 2);
    assert!(error_message.unwrap().contains("cannot open"));
}

#[test]
fn freeing_null_is_a_no_op() {
    // SAFETY: documented to accept NULL.
    unsafe { ofd_quicklook_result_free(ptr::null_mut()) };
}
