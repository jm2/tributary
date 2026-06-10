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
const WM_GETMINMAXINFO: u32 = 0x0024;
const HTMAXBUTTON: isize = 9;

const MONITOR_DEFAULTTONEAREST: u32 = 0x0002;

const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;

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
    fn GetDpiForWindow(hwnd: *mut c_void) -> u32;
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
                    // The rect is stored in GTK *logical* pixels, but the
                    // cursor coords from ScreenToClient are *physical*
                    // device pixels (GTK4 is per-monitor DPI aware on
                    // Windows). Scale the rect to physical pixels using the
                    // window's DPI so the hit-test lines up at any display
                    // scale (e.g. 150% / 200%), not just 100%.
                    // SAFETY: GetDpiForWindow is a stateless query on a
                    // valid HWND; it returns 0 only on failure (→ scale 1.0).
                    let dpi = unsafe { GetDpiForWindow(hwnd) };
                    let scale = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
                    let px = (f64::from(x) * scale) as i32;
                    let py = (f64::from(y) * scale) as i32;
                    let pw = (f64::from(w) * scale) as i32;
                    let ph = (f64::from(h) * scale) as i32;
                    if pt.x >= px && pt.x <= px + pw && pt.y >= py && pt.y <= py + ph {
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
