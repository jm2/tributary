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
const WM_NCCALCSIZE: u32 = 0x0083;
const WM_NCHITTEST: u32 = 0x0084;
const WM_NCDESTROY: u32 = 0x0082;
const WM_GETMINMAXINFO: u32 = 0x0024;
const HTMAXBUTTON: isize = 9;

const MONITOR_DEFAULTTONEAREST: u32 = 0x0002;

const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;

// Window styles required for Windows 11 Snap Assist to include the window
// in its quadrant-fill picker. GTK4 CSD on Windows strips most of these
// because it draws its own title bar; we add them back and suppress the
// resulting OS-drawn frame via WM_NCCALCSIZE.
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_THICKFRAME: u32 = 0x0004_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_MINIMIZEBOX: u32 = 0x0002_0000;
const WS_MAXIMIZEBOX: u32 = 0x0001_0000;

// SetWindowPos flags.
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_FRAMECHANGED: u32 = 0x0020;

const SUBCLASS_ID: usize = 0x5472_6962; // "Trib" in hex

// Win32 FFI declarations — using raw types to avoid a `windows` crate dependency.
//
// `SetWindowSubclass` / `RemoveWindowSubclass` / `DefSubclassProc` live in
// `comctl32.dll`, which is not auto-linked by the Rust toolchain. MinGW
// happens to pull it in today via implicit defaults, but the explicit
// `#[link]` attribute makes the dependency intentional and protects against
// future toolchain changes.
#[link(name = "comctl32")]
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
}

#[allow(non_snake_case)]
extern "system" {
    fn GetCursorPos(point: *mut Point) -> i32;
    fn ScreenToClient(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn MonitorFromWindow(hwnd: *mut c_void, dwFlags: u32) -> *mut c_void;
    fn GetMonitorInfoW(hMonitor: *mut c_void, lpmi: *mut MonitorInfo) -> i32;
    fn GetWindowLongPtrW(hwnd: *mut c_void, nIndex: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: *mut c_void, nIndex: i32, dwNewLong: isize) -> isize;
    fn SetWindowPos(
        hwnd: *mut c_void,
        hwnd_insert_after: *mut c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        uFlags: u32,
    ) -> i32;
}

/// Win32 POINT structure (client coordinates).
#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}

/// Win32 RECT structure.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// Win32 MONITORINFO structure.
#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: Rect,
    rc_work: Rect,
    dw_flags: u32,
}

/// Win32 MINMAXINFO structure (passed via `lparam` on `WM_GETMINMAXINFO`).
#[repr(C)]
struct MinMaxInfo {
    pt_reserved: Point,
    pt_max_size: Point,
    pt_max_position: Point,
    pt_min_track_size: Point,
    pt_max_track_size: Point,
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

    // Diagnostic: dump the window styles so we can see what GTK4 actually
    // sets on Windows. Snap Assist's "fill the other quadrants" picker only
    // includes windows that look like a normal top-level (WS_THICKFRAME +
    // WS_MAXIMIZEBOX present, WS_EX_TOOLWINDOW absent). If GTK's CSD is
    // missing one of these, that's the root of the snap-list-omission bug.
    // SAFETY: GetWindowLongPtrW is a stateless query against a valid HWND.
    let (style, ex_style) = unsafe {
        (
            GetWindowLongPtrW(hwnd, GWL_STYLE),
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE),
        )
    };
    tracing::info!(
        style = format!("{:#010x}", style as u32),
        ex_style = format!("{:#010x}", ex_style as u32),
        ws_thickframe = (style as u32 & 0x0004_0000) != 0,
        ws_maximizebox = (style as u32 & 0x0001_0000) != 0,
        ws_caption = (style as u32 & 0x00C0_0000) == 0x00C0_0000,
        ws_ex_toolwindow = (ex_style as u32 & 0x0000_0080) != 0,
        "Window styles before Snap Layout subclass install"
    );

    // SAFETY: SetWindowSubclass is a standard Win32 API. We pass a valid HWND
    // (extracted by gdk4-win32), a static extern fn, and a unique subclass ID.
    // The callback is stateless — no heap data is referenced via dwRefData.
    unsafe {
        SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, 0);
    }

    // Restore the window styles that GTK4 CSD strips on Windows. Snap Assist
    // only includes a window in its "fill the other quadrants" picker if the
    // HWND looks like a normal top-level: WS_CAPTION + WS_THICKFRAME +
    // WS_MAXIMIZEBOX + WS_SYSMENU + WS_MINIMIZEBOX. WS_THICKFRAME is also
    // required for the maximize-button hover flyout.
    //
    // Adding WS_CAPTION normally tells Windows to draw a native title bar in
    // the non-client area — but our subclass intercepts WM_NCCALCSIZE and
    // returns 0, which collapses the non-client area to nothing, so GTK's
    // CSD continues to own the entire client area. The OS just *believes*
    // there's a frame, which is enough for Snap Assist eligibility.
    //
    // SAFETY: standard Win32 calls on a valid HWND.
    unsafe {
        let new_style = (style as u32)
            | WS_CAPTION
            | WS_THICKFRAME
            | WS_SYSMENU
            | WS_MINIMIZEBOX
            | WS_MAXIMIZEBOX;
        if (new_style as isize) != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, new_style as isize);
            // SWP_FRAMECHANGED forces the window to recompute its frame,
            // which resends WM_NCCALCSIZE. Our subclass is already installed,
            // so it will swallow the resulting non-client area to keep CSD intact.
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
            );
            tracing::info!(
                old_style = format!("{:#010x}", style as u32),
                new_style = format!("{:#010x}", new_style),
                "Restored snap-eligible window styles"
            );
        }
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
        WM_NCCALCSIZE => {
            // Adding WS_CAPTION + WS_THICKFRAME makes the window snap-eligible,
            // but Windows would then draw a native title bar in the non-client
            // area. Returning 0 (for either wparam form) tells Windows the
            // client area equals the proposed window rect — i.e., zero
            // non-client area — so GTK4's CSD continues to render the entire
            // window unchanged. The OS believes there's a frame for snap
            // purposes; nothing actually gets drawn there.
            0
        }
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
        WM_GETMINMAXINFO => {
            // Clip the maximised window's bounds to the monitor's *work area*
            // (screen rect minus taskbar / docked appbars) so a maximised
            // window doesn't overhang the taskbar. GTK4 CSD on Windows
            // doesn't override this message, so without this handler the
            // default tracking size lets the window cover the taskbar until
            // the next move forces Windows to re-evaluate.
            //
            // SAFETY: lparam on WM_GETMINMAXINFO is always a valid
            // pointer to a MINMAXINFO supplied by the OS. MonitorFromWindow
            // returns NULL only on invalid HWND; we null-check before deref.
            unsafe {
                let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                if !monitor.is_null() {
                    let mut mi = MonitorInfo {
                        cb_size: std::mem::size_of::<MonitorInfo>() as u32,
                        rc_monitor: Rect::default(),
                        rc_work: Rect::default(),
                        dw_flags: 0,
                    };
                    if GetMonitorInfoW(monitor, &mut mi) != 0 {
                        let mmi = lparam as *mut MinMaxInfo;
                        if !mmi.is_null() {
                            // ptMaxPosition is expressed relative to the
                            // primary monitor's coordinate system.
                            (*mmi).pt_max_position.x = mi.rc_work.left - mi.rc_monitor.left;
                            (*mmi).pt_max_position.y = mi.rc_work.top - mi.rc_monitor.top;
                            (*mmi).pt_max_size.x = mi.rc_work.right - mi.rc_work.left;
                            (*mmi).pt_max_size.y = mi.rc_work.bottom - mi.rc_work.top;
                            // Cap the user-resizable maximum at the work
                            // area too, so dragging-resize can't exceed it.
                            (*mmi).pt_max_track_size.x = mi.rc_work.right - mi.rc_work.left;
                            (*mmi).pt_max_track_size.y = mi.rc_work.bottom - mi.rc_work.top;
                            return 0;
                        }
                    }
                }
            }
            // SAFETY: fall through to the default handler if any of the
            // monitor queries failed.
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
