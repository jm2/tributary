//! Windows 11 Snap Layout support via `WM_NCHITTEST` subclassing.
//!
//! GTK4's Client-Side Decorations draw their own title bar, so Windows
//! doesn't recognise the maximize button for Snap Layouts. This module
//! installs a Win32 window subclass that returns `HTMAXBUTTON` when the
//! cursor hovers over the maximize button area, enabling the native
//! Windows 11 Snap Layout flyout.
//!
//! # Safety
//!
//! The `unsafe` surface is minimal and well-contained:
//! - A stateless `extern "system"` callback (~15 lines).
//! - `SetWindowSubclass` / `RemoveWindowSubclass` lifecycle.
//! - No memory allocation, no pointer arithmetic, no closures captured.
//!
//! The callback is read-only (hit-test override). Worst case: snap menu
//! doesn't appear and `DefSubclassProc` handles the message normally.

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::sync::Mutex;

// Win32 constants.
const WM_NCHITTEST: u32 = 0x0084;
const WM_NCDESTROY: u32 = 0x0082;
const HTMAXBUTTON: isize = 9;

const SUBCLASS_ID: usize = 0x5472_6962; // "Trib" in hex

// Win32 FFI declarations — using raw types to avoid a `windows` crate dependency.
#[allow(non_snake_case)]
extern "system" {
    fn SetWindowSubclass(
        hwnd: *mut c_void,
        pfnSubclass: unsafe extern "system" fn(
            *mut c_void,
            u32,
            usize,
            isize,
            usize,
            usize,
        ) -> isize,
        uIdSubclass: usize,
        dwRefData: usize,
    ) -> i32;

    fn RemoveWindowSubclass(
        hwnd: *mut c_void,
        pfnSubclass: unsafe extern "system" fn(
            *mut c_void,
            u32,
            usize,
            isize,
            usize,
            usize,
        ) -> isize,
        uIdSubclass: usize,
    ) -> i32;

    fn DefSubclassProc(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> isize;

    fn GetCursorPos(point: *mut Point) -> i32;
    fn ScreenToClient(hwnd: *mut c_void, point: *mut Point) -> i32;
}

/// Win32 POINT structure (client coordinates).
#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

/// Stored maximize button rectangle (client coordinates).
/// Updated by the GTK side whenever the header bar layout changes.
static MAX_BUTTON_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

/// Enable Windows 11 Snap Layout support for the given window.
///
/// `maximize_rect` is `(x, y, width, height)` in client coordinates
/// of the maximize/restore button in the header bar.
///
/// Call this once after the window has been realised and the HWND extracted.
pub fn enable_snap_layout(hwnd: *mut c_void, maximize_rect: (i32, i32, i32, i32)) {
    // Store the initial button rect.
    if let Ok(mut rect) = MAX_BUTTON_RECT.lock() {
        *rect = Some(maximize_rect);
    }

    // SAFETY: SetWindowSubclass is a standard Win32 API. We pass a valid HWND
    // (extracted by gdk4-win32), a static extern fn, and a unique subclass ID.
    // The callback is stateless — no heap data is referenced via dwRefData.
    unsafe {
        SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, 0);
    }

    tracing::info!("Windows 11 Snap Layout subclass installed");
}

/// Update the maximize button rectangle (call when the header bar is resized).
pub fn update_maximize_rect(rect: (i32, i32, i32, i32)) {
    if let Ok(mut r) = MAX_BUTTON_RECT.lock() {
        *r = Some(rect);
    }
}

/// The Win32 subclass callback.
///
/// Intercepts `WM_NCHITTEST` to return `HTMAXBUTTON` when the cursor
/// is inside the maximize button area. All other messages (and all
/// `WM_NCHITTEST` outside the button) are forwarded to `DefSubclassProc`.
///
/// # Safety
///
/// This is an `extern "system"` callback invoked by the Windows message
/// loop. It must not panic (panicking across FFI is UB). All branches
/// either return a constant or call `DefSubclassProc`.
unsafe extern "system" fn subclass_proc(
    hwnd: *mut c_void,
    msg: u32,
    wparam: usize,
    lparam: isize,
    _uid: usize,
    _ref_data: usize,
) -> isize {
    match msg {
        WM_NCHITTEST => {
            // Get cursor position in client coordinates.
            let mut pt = Point::default();
            // SAFETY: GetCursorPos and ScreenToClient are standard Win32.
            unsafe {
                GetCursorPos(&mut pt);
                ScreenToClient(hwnd, &mut pt);
            }

            // Check if cursor is within the maximize button rect.
            if let Ok(guard) = MAX_BUTTON_RECT.lock() {
                if let Some((x, y, w, h)) = *guard {
                    if pt.x >= x && pt.x <= x + w && pt.y >= y && pt.y <= y + h {
                        return HTMAXBUTTON;
                    }
                }
            }

            // Outside the button — let GTK handle it.
            // SAFETY: DefSubclassProc forwards to the original wndproc.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_NCDESTROY => {
            // Clean up: remove our subclass before the window is destroyed.
            // SAFETY: RemoveWindowSubclass with the same fn + ID we registered.
            unsafe {
                RemoveWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID);
            }
            tracing::debug!("Snap Layout subclass removed (WM_NCDESTROY)");
            // SAFETY: Forward to the next handler in the chain.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        _ => {
            // SAFETY: Forward all other messages unchanged.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
    }
}
