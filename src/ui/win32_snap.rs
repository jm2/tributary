//! Windows 11 Snap Layout support via `WM_NCHITTEST` subclassing.
//!
//! GTK4's Client-Side Decorations draw their own title bar, so Windows
//! doesn't recognise the maximize button for Snap Layouts. This module
//! installs a Win32 window subclass that returns `HTMAXBUTTON` when the
//! cursor hovers over GTK's actual maximize button allocation. This gives
//! the Windows shell the documented hook it needs to offer Snap Layouts.
//!
//! # Safety
//!
//! The `unsafe` surface is minimal and well-contained:
//! - An `extern "system"` callback backed by per-HWND immutable ownership.
//! - `SetWindowSubclass` / `RemoveWindowSubclass` lifecycle.
//! - No GTK calls, Rust heap allocation, or panic paths inside the callback.
//!
//! The callback never changes frame styles. It owns maximize-button clicks and
//! constrains the maximized work-area rectangle. During GTK's manual CSD sizing
//! loop it clips only a dragged edge that enters a taskbar/appbar strip; it
//! deliberately leaves all other tracking dimensions unrestricted.

#![cfg(target_os = "windows")]

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use adw::prelude::*;
use gtk::glib;

// Win32 constants.
const WM_NCHITTEST: u32 = 0x0084;
const WM_NCDESTROY: u32 = 0x0082;
const WM_NCMOUSELEAVE: u32 = 0x02A2;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_WINDOWPOSCHANGING: u32 = 0x0046;
const WM_NCLBUTTONDOWN: u32 = 0x00A1;
const WM_NCLBUTTONUP: u32 = 0x00A2;
const HTMAXBUTTON: isize = 9;

const MONITOR_DEFAULTTONEAREST: u32 = 0x0002;
const SW_MAXIMIZE: i32 = 3;
const SW_RESTORE: i32 = 9;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const VK_LBUTTON: i32 = 0x01;

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
    fn GetAsyncKeyState(virtual_key: i32) -> i16;
    fn GetCapture() -> *mut c_void;
    fn GetCursorPos(point: *mut Point) -> i32;
    fn GetWindowRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    fn ScreenToClient(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn MonitorFromPoint(point: Point, flags: u32) -> *mut c_void;
    fn MonitorFromWindow(hwnd: *mut c_void, flags: u32) -> *mut c_void;
    fn GetMonitorInfoW(monitor: *mut c_void, info: *mut MonitorInfo) -> i32;
    fn IsZoomed(hwnd: *mut c_void) -> i32;
    fn ShowWindow(hwnd: *mut c_void, command: i32) -> i32;
}

#[link(name = "dwmapi")]
#[allow(non_snake_case)]
extern "system" {
    fn DwmDefWindowProc(
        hwnd: *mut c_void,
        msg: u32,
        wparam: usize,
        lparam: isize,
        result: *mut isize,
    ) -> i32;
}

/// Win32 POINT structure (client coordinates).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MonitorInfo {
    size: u32,
    monitor: Rect,
    work_area: Rect,
    flags: u32,
}

#[repr(C)]
struct MinMaxInfo {
    reserved: Point,
    max_size: Point,
    max_position: Point,
    min_track_size: Point,
    max_track_size: Point,
}

#[repr(C)]
struct WindowPos {
    hwnd: *mut c_void,
    insert_after: *mut c_void,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    flags: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HorizontalEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VerticalEdge {
    Top,
    Bottom,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SizingEdges {
    horizontal: Option<HorizontalEdge>,
    vertical: Option<VerticalEdge>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CsdInsets {
    left: i32,
    right: i32,
    top: i32,
    bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HitRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl HitRect {
    fn contains(self, point: Point) -> bool {
        point.x >= self.left && point.x < self.right && point.y >= self.top && point.y < self.bottom
    }
}

struct SnapState {
    maximize_rect: Mutex<Option<HitRect>>,
    csd_insets: Mutex<CsdInsets>,
    update_queued: AtomicBool,
}

impl SnapState {
    fn new() -> Self {
        Self {
            maximize_rect: Mutex::new(None),
            csd_insets: Mutex::new(CsdInsets::default()),
            update_queued: AtomicBool::new(false),
        }
    }

    fn set_maximize_rect(&self, rect: Option<HitRect>) {
        if let Ok(mut current) = self.maximize_rect.lock() {
            *current = rect;
        }
    }

    fn is_over_maximize(&self, point: Point) -> bool {
        self.maximize_rect
            .lock()
            .ok()
            .and_then(|rect| *rect)
            .is_some_and(|rect| rect.contains(point))
    }

    fn set_csd_insets(&self, insets: CsdInsets) {
        if let Ok(mut current) = self.csd_insets.lock() {
            *current = insets;
        }
    }

    fn csd_insets(&self) -> CsdInsets {
        self.csd_insets
            .lock()
            .map_or_else(|_| CsdInsets::default(), |insets| *insets)
    }
}

fn find_maximize_button(widget: &gtk::Widget) -> Option<gtk::Widget> {
    if widget.has_css_class("maximize") && widget.downcast_ref::<gtk::Button>().is_some() {
        return Some(widget.clone());
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        if let Some(button) = find_maximize_button(&current) {
            return Some(button);
        }
        child = current.next_sibling();
    }
    None
}

fn physical_hit_rect(
    logical_x: f64,
    logical_y: f64,
    logical_width: f64,
    logical_height: f64,
    surface_transform: (f64, f64),
    scale: f64,
) -> Option<HitRect> {
    if logical_width <= 0.0 || logical_height <= 0.0 {
        return None;
    }

    let scale = scale.max(1.0);
    // GtkNative's surface transform is the translation GTK itself applies
    // when snapshotting widget coordinates into its GdkSurface. Add it before
    // converting the logical surface rectangle to Win32 client pixels.
    let left = ((logical_x + surface_transform.0) * scale).floor() as i32;
    let top = ((logical_y + surface_transform.1) * scale).floor() as i32;
    let right = ((logical_x + logical_width + surface_transform.0) * scale).ceil() as i32;
    let bottom = ((logical_y + logical_height + surface_transform.1) * scale).ceil() as i32;

    (right > left && bottom > top).then_some(HitRect {
        left,
        top,
        right,
        bottom,
    })
}

fn physical_csd_insets(
    surface_width: i32,
    surface_height: i32,
    widget_width: i32,
    widget_height: i32,
    surface_transform: (f64, f64),
    scale: f64,
) -> CsdInsets {
    let scale = scale.max(1.0);
    let left = surface_transform.0.max(0.0);
    let top = surface_transform.1.max(0.0);
    let right = (f64::from(surface_width) - left - f64::from(widget_width)).max(0.0);
    let bottom = (f64::from(surface_height) - top - f64::from(widget_height)).max(0.0);

    // Round toward the widget on every edge: an uncertain fractional pixel is
    // kept out of the appbar strip rather than allowing painted content into it.
    CsdInsets {
        left: (left * scale).floor() as i32,
        right: (right * scale).floor() as i32,
        top: (top * scale).floor() as i32,
        bottom: (bottom * scale).floor() as i32,
    }
}

fn current_csd_insets(window: &adw::ApplicationWindow) -> CsdInsets {
    let Some(surface) = window.surface() else {
        return CsdInsets::default();
    };

    physical_csd_insets(
        surface.width(),
        surface.height(),
        window.width(),
        window.height(),
        window.surface_transform(),
        surface.scale(),
    )
}

fn current_maximize_rect(
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
) -> Option<HitRect> {
    let button = find_maximize_button(header.upcast_ref())?;
    if !button.is_mapped() {
        return None;
    }

    let bounds = button.compute_bounds(window)?;
    let surface = window.surface()?;
    physical_hit_rect(
        f64::from(bounds.x()),
        f64::from(bounds.y()),
        f64::from(bounds.width()),
        f64::from(bounds.height()),
        window.surface_transform(),
        surface.scale(),
    )
}

fn queue_maximize_rect_update(
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
    state: &Arc<SnapState>,
) {
    if state.update_queued.swap(true, Ordering::AcqRel) {
        return;
    }

    let window = window.downgrade();
    let header = header.downgrade();
    let state = Arc::clone(state);
    glib::idle_add_local_once(move || {
        state.update_queued.store(false, Ordering::Release);
        let Some((window, header)) = window.upgrade().zip(header.upgrade()) else {
            state.set_maximize_rect(None);
            state.set_csd_insets(CsdInsets::default());
            return;
        };

        state.set_maximize_rect(current_maximize_rect(&window, &header));
        state.set_csd_insets(current_csd_insets(&window));
    });
}

fn track_maximize_rect(
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
    state: &Arc<SnapState>,
) {
    if let Some(surface) = window.surface() {
        let window_weak = window.downgrade();
        let header_weak = header.downgrade();
        let state_for_layout = Arc::clone(state);
        surface.connect_layout(move |_, _, _| {
            if let Some((window, header)) = window_weak.upgrade().zip(header_weak.upgrade()) {
                queue_maximize_rect_update(&window, &header, &state_for_layout);
            }
        });

        let window_weak = window.downgrade();
        let header_weak = header.downgrade();
        let state_for_scale = Arc::clone(state);
        surface.connect_scale_notify(move |_| {
            if let Some((window, header)) = window_weak.upgrade().zip(header_weak.upgrade()) {
                queue_maximize_rect_update(&window, &header, &state_for_scale);
            }
        });
    }

    let header_weak = header.downgrade();
    let state_for_maximize = Arc::clone(state);
    window.connect_maximized_notify(move |window| {
        if let Some(header) = header_weak.upgrade() {
            queue_maximize_rect_update(window, &header, &state_for_maximize);
        }
    });

    let window_weak = window.downgrade();
    let state_for_layout = Arc::clone(state);
    header.connect_decoration_layout_notify(move |header| {
        if let Some(window) = window_weak.upgrade() {
            queue_maximize_rect_update(&window, header, &state_for_layout);
        }
    });

    queue_maximize_rect_update(window, header, state);
}

fn apply_monitor_work_area(info: &mut MinMaxInfo, monitor: Rect, work_area: Rect) {
    info.max_position.x = work_area.left - monitor.left;
    info.max_position.y = work_area.top - monitor.top;
    info.max_size.x = work_area.right - work_area.left;
    info.max_size.y = work_area.bottom - work_area.top;
}

fn apply_sizing_work_area(
    rect: &mut Rect,
    edges: SizingEdges,
    monitor: Rect,
    work_area: Rect,
    insets: CsdInsets,
) -> bool {
    let original = *rect;

    // Only an inset between rcMonitor and rcWork represents a taskbar or
    // another docked appbar. Do not clamp ordinary monitor edges: doing so
    // would prevent a restored window from spanning monitors and would
    // recreate the portrait-monitor width regression caused by globally
    // capping MINMAXINFO::max_track_size.
    if work_area.left > monitor.left && edges.horizontal == Some(HorizontalEdge::Left) {
        rect.left = rect.left.max(work_area.left.saturating_sub(insets.left));
    }
    if work_area.right < monitor.right && edges.horizontal == Some(HorizontalEdge::Right) {
        rect.right = rect.right.min(work_area.right.saturating_add(insets.right));
    }
    if work_area.top > monitor.top && edges.vertical == Some(VerticalEdge::Top) {
        rect.top = rect.top.max(work_area.top.saturating_sub(insets.top));
    }
    if work_area.bottom < monitor.bottom && edges.vertical == Some(VerticalEdge::Bottom) {
        rect.bottom = rect
            .bottom
            .min(work_area.bottom.saturating_add(insets.bottom));
    }

    *rect != original
}

fn active_sizing_edges(current: Rect, proposed: Rect, cursor: Point) -> SizingEdges {
    let mut edges = SizingEdges::default();
    let current_width = current.right - current.left;
    let proposed_width = proposed.right - proposed.left;
    let current_height = current.bottom - current.top;
    let proposed_height = proposed.bottom - proposed.top;

    if current_width != proposed_width {
        if proposed.left != current.left && proposed.right == current.right {
            edges.horizontal = Some(HorizontalEdge::Left);
        } else if proposed.left == current.left && proposed.right != current.right {
            edges.horizontal = Some(HorizontalEdge::Right);
        } else {
            // Style/DPI bookkeeping can shift both outer bounds by a pixel.
            // In that ambiguous case, the pointer remains on the edge GTK is
            // actively dragging, so proximity identifies the intended side.
            let left_distance = (i64::from(cursor.x) - i64::from(proposed.left)).abs();
            let right_distance = (i64::from(cursor.x) - i64::from(proposed.right)).abs();
            if left_distance <= right_distance {
                edges.horizontal = Some(HorizontalEdge::Left);
            } else {
                edges.horizontal = Some(HorizontalEdge::Right);
            }
        }
    }

    if current_height != proposed_height {
        if proposed.top != current.top && proposed.bottom == current.bottom {
            edges.vertical = Some(VerticalEdge::Top);
        } else if proposed.top == current.top && proposed.bottom != current.bottom {
            edges.vertical = Some(VerticalEdge::Bottom);
        } else {
            let top_distance = (i64::from(cursor.y) - i64::from(proposed.top)).abs();
            let bottom_distance = (i64::from(cursor.y) - i64::from(proposed.bottom)).abs();
            if top_distance <= bottom_distance {
                edges.vertical = Some(VerticalEdge::Top);
            } else {
                edges.vertical = Some(VerticalEdge::Bottom);
            }
        }
    }

    edges
}

unsafe fn constrain_maximized_window_to_work_area(hwnd: *mut c_void, lparam: isize) {
    if lparam == 0 {
        return;
    }

    // SAFETY: hwnd belongs to the current window procedure. The nearest
    // monitor fallback also covers transient monitor reconfiguration.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return;
    }

    let mut monitor_info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        monitor: Rect::default(),
        work_area: Rect::default(),
        flags: 0,
    };
    // SAFETY: monitor is valid and monitor_info has the documented size.
    if unsafe { GetMonitorInfoW(monitor, &raw mut monitor_info) } == 0 {
        return;
    }

    // SAFETY: WM_GETMINMAXINFO supplies a writable MINMAXINFO pointer. GTK has
    // already populated it; change only maximized bounds and deliberately
    // preserve both tracking-size fields so ordinary resizing remains free.
    let info = unsafe { &mut *(lparam as *mut MinMaxInfo) };
    apply_monitor_work_area(info, monitor_info.monitor, monitor_info.work_area);
}

unsafe fn constrain_interactive_window_pos_to_work_area(
    hwnd: *mut c_void,
    lparam: isize,
    state: &SnapState,
) -> bool {
    if lparam == 0 {
        return false;
    }

    // GTK 4.22 implements CSD resizing with a pointer grab plus repeated
    // SetWindowPos calls instead of Windows' modal sizing loop. Restrict this
    // hook to that active left-button grab so programmatic layout, maximize,
    // Snap, initial placement, and ordinary moves keep their native behavior.
    // SAFETY: both calls are stateless queries on the current UI thread.
    if unsafe { GetCapture() } != hwnd || unsafe { GetAsyncKeyState(VK_LBUTTON) } >= 0 {
        return false;
    }

    // SAFETY: WM_WINDOWPOSCHANGING supplies a writable WINDOWPOS for the
    // duration of this synchronous callback.
    let position = unsafe { &mut *(lparam as *mut WindowPos) };
    if position.flags & SWP_NOSIZE != 0 {
        return false;
    }

    let mut current = Rect::default();
    // SAFETY: hwnd is live and current points to writable RECT storage.
    if unsafe { GetWindowRect(hwnd, &raw mut current) } == 0 {
        return false;
    }

    let left = if position.flags & SWP_NOMOVE != 0 {
        current.left
    } else {
        position.x
    };
    let top = if position.flags & SWP_NOMOVE != 0 {
        current.top
    } else {
        position.y
    };
    let mut proposed = Rect {
        left,
        top,
        right: left.saturating_add(position.width),
        bottom: top.saturating_add(position.height),
    };

    if proposed.right - proposed.left == current.right - current.left
        && proposed.bottom - proposed.top == current.bottom - current.top
    {
        return false;
    }

    let mut cursor = Point::default();
    // SAFETY: cursor points to writable storage for a screen-space POINT.
    if unsafe { GetCursorPos(&raw mut cursor) } == 0 {
        return false;
    }

    // The active sizing cursor identifies which monitor's taskbar matters
    // when the proposed window rectangle spans more than one monitor. All
    // coordinates supplied by these APIs are in the same physical screen
    // coordinate space, so no GTK scale or DPI conversion belongs here.
    // SAFETY: MONITOR_DEFAULTTONEAREST guarantees a monitor for a valid point.
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return false;
    }

    let mut monitor_info = MonitorInfo {
        size: std::mem::size_of::<MonitorInfo>() as u32,
        monitor: Rect::default(),
        work_area: Rect::default(),
        flags: 0,
    };
    // SAFETY: monitor is valid and monitor_info has the documented size.
    if unsafe { GetMonitorInfoW(monitor, &raw mut monitor_info) } == 0 {
        return false;
    }

    let edges = active_sizing_edges(current, proposed, cursor);
    if !apply_sizing_work_area(
        &mut proposed,
        edges,
        monitor_info.monitor,
        monitor_info.work_area,
        state.csd_insets(),
    ) {
        return false;
    }

    if position.flags & SWP_NOMOVE == 0 {
        position.x = proposed.left;
        position.y = proposed.top;
    }
    position.width = proposed.right - proposed.left;
    position.height = proposed.bottom - proposed.top;
    true
}

unsafe fn toggle_maximized(hwnd: *mut c_void) {
    // Returning HTMAXBUTTON redirects the click away from GTK's client-side
    // button. Drive the same native maximize/restore state transition here;
    // GDK observes the resulting window-state messages and updates GTK.
    let command = if unsafe { IsZoomed(hwnd) } != 0 {
        SW_RESTORE
    } else {
        SW_MAXIMIZE
    };
    // SAFETY: hwnd is the live window currently handling the button message.
    unsafe {
        ShowWindow(hwnd, command);
    }
}

/// Enable Windows 11 Snap Layout support for the given window.
///
/// Call this once after the window has been realised and the HWND extracted.
pub fn enable_snap_layout(
    hwnd: *mut c_void,
    window: &adw::ApplicationWindow,
    header: &adw::HeaderBar,
) -> bool {
    let state = Arc::new(SnapState::new());
    state.set_maximize_rect(current_maximize_rect(window, header));
    state.set_csd_insets(current_csd_insets(window));
    let subclass_state = Arc::into_raw(Arc::clone(&state));

    // SAFETY: SetWindowSubclass is a standard Win32 API. The raw Arc owns one
    // state reference until WM_NCDESTROY; all GTK-facing signal handlers keep
    // independent Arc references and never enter the native callback.
    let installed =
        unsafe { SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, subclass_state as usize) };
    if installed == 0 {
        // SAFETY: SetWindowSubclass did not retain the pointer, so reclaim the
        // one reference created by Arc::into_raw above.
        drop(unsafe { Arc::from_raw(subclass_state) });
        tracing::warn!("Failed to install Windows 11 Snap Layout subclass");
        return false;
    }

    track_maximize_rect(window, header, &state);
    tracing::info!("Windows 11 Snap Layout subclass installed");
    true
}

fn point_from_lparam(lparam: isize) -> Point {
    let packed = lparam as u32;
    Point {
        x: i32::from(packed as u16 as i16),
        y: i32::from((packed >> 16) as u16 as i16),
    }
}

unsafe fn dwm_result(hwnd: *mut c_void, msg: u32, wparam: usize, lparam: isize) -> Option<isize> {
    let mut result = 0;
    // SAFETY: hwnd and message parameters come directly from the window
    // procedure; result points to a live LRESULT for the duration of the call.
    let handled = unsafe { DwmDefWindowProc(hwnd, msg, wparam, lparam, &raw mut result) };
    (handled != 0).then_some(result)
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
    ref_data: usize,
) -> isize {
    if matches!(msg, WM_NCHITTEST | WM_NCMOUSELEAVE) {
        // Windows requires custom frames to give DWM first refusal for
        // non-client caption-button messages, including mouse leave.
        // SAFETY: all arguments came directly from the native window proc.
        if let Some(result) = unsafe { dwm_result(hwnd, msg, wparam, lparam) } {
            return result;
        }
    }

    match msg {
        WM_GETMINMAXINFO => {
            // Preserve GTK's minimum size, virtual-desktop tracking maximum,
            // and CSD bookkeeping, then correct only the maximized outer
            // rectangle so it cannot cover the taskbar or another appbar.
            // SAFETY: DefSubclassProc handles the message synchronously.
            let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            // SAFETY: lparam is the OS-owned MINMAXINFO for this message.
            unsafe {
                constrain_maximized_window_to_work_area(hwnd, lparam);
            }
            result
        }
        WM_WINDOWPOSCHANGING => {
            // GTK performs CSD resize drags with SetWindowPos rather than the
            // native WM_SIZING loop. Let GTK process the proposed placement,
            // then clip only the active edge if it enters an appbar strip.
            // SAFETY: DefSubclassProc handles the message synchronously.
            let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            // SAFETY: lparam is the OS-owned WINDOWPOS for this message.
            if ref_data != 0 {
                // SAFETY: dwRefData owns this state until WM_NCDESTROY.
                let state = unsafe { &*(ref_data as *const SnapState) };
                unsafe {
                    constrain_interactive_window_pos_to_work_area(hwnd, lparam, state);
                }
            }
            result
        }
        WM_NCHITTEST => {
            // WM_NCHITTEST packs signed screen coordinates into lparam. Using
            // the message coordinates (rather than sampling the cursor again)
            // also works on monitors with negative virtual-screen origins.
            let mut point = point_from_lparam(lparam);
            // SAFETY: ScreenToClient mutates a live POINT for this valid HWND.
            if unsafe { ScreenToClient(hwnd, &raw mut point) } == 0 {
                // SAFETY: DefSubclassProc forwards to the original wndproc.
                return unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            }

            if ref_data != 0 {
                // SAFETY: enable_snap_layout stores one Arc-owned SnapState in
                // dwRefData and reclaims it only in WM_NCDESTROY below.
                let state = unsafe { &*(ref_data as *const SnapState) };
                if state.is_over_maximize(point) {
                    return HTMAXBUTTON;
                }
            }

            // Outside the button — let GTK handle it.
            // SAFETY: DefSubclassProc forwards to the original wndproc.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        WM_NCLBUTTONDOWN if wparam == HTMAXBUTTON as usize => {
            // We own the matching button-up because HTMAXBUTTON prevents GTK
            // from receiving its ordinary client-side pointer sequence.
            0
        }
        WM_NCLBUTTONUP if wparam == HTMAXBUTTON as usize => {
            // SAFETY: this is the live HWND associated with the hit-tested
            // maximize button.
            unsafe {
                toggle_maximized(hwnd);
            }
            0
        }
        WM_NCDESTROY => {
            // Clean up: remove our subclass before the window is destroyed.
            // SAFETY: RemoveWindowSubclass with the same fn + ID we registered.
            unsafe {
                RemoveWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID);
            }
            // SAFETY: Forward to the next handler in the chain.
            let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            if ref_data != 0 {
                // SAFETY: this exactly balances Arc::into_raw at successful
                // installation, and WM_NCDESTROY is delivered once per HWND.
                drop(unsafe { Arc::from_raw(ref_data as *const SnapState) });
            }
            result
        }
        _ => {
            // SAFETY: Forward all other messages unchanged.
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        active_sizing_edges, apply_monitor_work_area, apply_sizing_work_area, physical_csd_insets,
        physical_hit_rect, point_from_lparam, CsdInsets, HitRect, HorizontalEdge, MinMaxInfo,
        Point, Rect, SizingEdges, VerticalEdge,
    };

    fn pack_point(x: i16, y: i16) -> isize {
        (((y as u16 as u32) << 16) | u32::from(x as u16)) as isize
    }

    #[test]
    fn hit_test_message_coordinates_stay_signed() {
        assert_eq!(
            point_from_lparam(pack_point(-1_920, -240)),
            Point { x: -1_920, y: -240 }
        );
        assert_eq!(
            point_from_lparam(pack_point(3_840, 1_080)),
            Point { x: 3_840, y: 1_080 }
        );
    }

    #[test]
    fn gtk_surface_transform_and_scale_are_applied_outward() {
        assert_eq!(
            physical_hit_rect(100.25, 4.5, 46.0, 36.0, (12.0, 8.0), 2.0),
            Some(HitRect {
                left: 224,
                top: 25,
                right: 317,
                bottom: 97,
            })
        );
    }

    #[test]
    fn hit_rect_uses_half_open_edges() {
        let rect = HitRect {
            left: 10,
            top: 20,
            right: 30,
            bottom: 40,
        };

        assert!(rect.contains(Point { x: 10, y: 20 }));
        assert!(rect.contains(Point { x: 29, y: 39 }));
        assert!(!rect.contains(Point { x: 30, y: 39 }));
        assert!(!rect.contains(Point { x: 29, y: 40 }));
    }

    #[test]
    fn gtk_surface_and_widget_sizes_yield_physical_csd_insets() {
        assert_eq!(
            physical_csd_insets(985, 541, 960, 516, (13.0, 13.0), 2.0),
            CsdInsets {
                left: 26,
                right: 24,
                top: 26,
                bottom: 24,
            }
        );
    }

    #[test]
    fn work_area_fix_preserves_both_manual_tracking_limits() {
        let mut info = MinMaxInfo {
            reserved: Point::default(),
            max_size: Point { x: 1, y: 2 },
            max_position: Point { x: 3, y: 4 },
            min_track_size: Point { x: 330, y: 240 },
            max_track_size: Point { x: 7_680, y: 4_320 },
        };

        apply_monitor_work_area(
            &mut info,
            Rect {
                left: 3_840,
                top: -1_080,
                right: 4_920,
                bottom: 840,
            },
            Rect {
                left: 3_888,
                top: -1_080,
                right: 4_920,
                bottom: 840,
            },
        );

        assert_eq!(info.max_position, Point { x: 48, y: 0 });
        assert_eq!(info.max_size, Point { x: 1_032, y: 1_920 });
        assert_eq!(info.min_track_size, Point { x: 330, y: 240 });
        assert_eq!(info.max_track_size, Point { x: 7_680, y: 4_320 });
    }

    #[test]
    fn bottom_taskbar_clamps_only_the_dragged_bottom_edge() {
        let monitor = Rect {
            left: 0,
            top: 0,
            right: 3_840,
            bottom: 2_160,
        };
        let work_area = Rect {
            bottom: 2_064,
            ..monitor
        };
        let mut proposed = Rect {
            left: -400,
            top: -200,
            right: 4_400,
            bottom: 2_200,
        };

        assert!(apply_sizing_work_area(
            &mut proposed,
            SizingEdges {
                horizontal: Some(HorizontalEdge::Right),
                vertical: Some(VerticalEdge::Bottom),
            },
            monitor,
            work_area,
            CsdInsets {
                left: 26,
                right: 24,
                top: 26,
                bottom: 24,
            },
        ));
        assert_eq!(
            proposed,
            Rect {
                left: -400,
                top: -200,
                right: 4_400,
                bottom: 2_088,
            }
        );
    }

    #[test]
    fn docked_appbar_clamps_only_when_its_edge_is_being_dragged() {
        let monitor = Rect {
            left: -2_160,
            top: 0,
            right: 0,
            bottom: 3_840,
        };
        let work_area = Rect {
            left: -2_064,
            ..monitor
        };
        let proposed = Rect {
            left: -2_200,
            top: -100,
            right: 200,
            bottom: 4_000,
        };

        let mut right_edge = proposed;
        assert!(!apply_sizing_work_area(
            &mut right_edge,
            SizingEdges {
                horizontal: Some(HorizontalEdge::Right),
                vertical: Some(VerticalEdge::Bottom),
            },
            monitor,
            work_area,
            CsdInsets {
                left: 24,
                ..CsdInsets::default()
            },
        ));
        assert_eq!(right_edge, proposed);

        let mut left_edge = proposed;
        assert!(apply_sizing_work_area(
            &mut left_edge,
            SizingEdges {
                horizontal: Some(HorizontalEdge::Left),
                vertical: None,
            },
            monitor,
            work_area,
            CsdInsets {
                left: 24,
                ..CsdInsets::default()
            },
        ));
        assert_eq!(left_edge.left, -2_088);
        assert_eq!(left_edge.right, proposed.right);
        assert_eq!(left_edge.top, proposed.top);
        assert_eq!(left_edge.bottom, proposed.bottom);
    }

    #[test]
    fn active_edges_preserve_the_fixed_sides_of_a_corner_drag() {
        let current = Rect {
            left: 100,
            top: 200,
            right: 1_100,
            bottom: 1_000,
        };
        let proposed = Rect {
            left: 100,
            top: 200,
            right: 1_400,
            bottom: 1_300,
        };

        assert_eq!(
            active_sizing_edges(current, proposed, Point { x: 1_400, y: 1_300 }),
            SizingEdges {
                horizontal: Some(HorizontalEdge::Right),
                vertical: Some(VerticalEdge::Bottom),
            }
        );
    }
}
