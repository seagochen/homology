//! The Windows title bar a program draws itself (Crescent's and
//! AnnotaryCat's, homology#28), the way VS Code, Chrome and Windows
//! Terminal draw theirs: the window keeps the system's frame (its shadow,
//! resize edges and snapping), the caption is taken into the client area,
//! and the window tells the system what each point of it is
//! (`WM_NCHITTEST`). The title bar's empty space is the caption: the system
//! moves the window, maximizes it on a double click and shows the window
//! menu on a right click. The maximize button is the maximize button, over
//! which Windows 11 shows its snap layouts. The top edge resizes.
//!
//! Moving onto a screen of another scale works too: winit 0.30 would place
//! the window as if it had the system's caption and bounce it at the edge
//! between two screens; the system's suggested place is taken instead
//! (`WM_DPICHANGED`). First written for AnnotaryCat (seagochen/annotary#152,
//! #199), then for Crescent (seagochen/crescent#49), now one copy for both.
//!
//! The program says where the caption and the maximize button are each
//! frame ([`set_regions`]); the rest of the window stays the program's. It
//! also keeps the program's window ([`window`]): what a system dialog (the
//! Microsoft Store's purchase window) is shown over. Elsewhere than Windows
//! every call does nothing and [`installed`] is false.
//!
//! The only crate here allowed `unsafe` code, in its `ffi` module alone: a
//! window's messages are handled only through Win32.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use raw_window_handle::HasWindowHandle;

/// A rectangle of the window's client area, in physical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Area {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Area {
    /// The area from the corners `min` (left, top) and `max` (right, bottom)
    /// in the UI's logical points, rounded to physical pixels at
    /// `pixels_per_point`: how a UI toolkit's rectangle becomes what
    /// [`set_regions`] takes.
    pub fn from_points(min: [f32; 2], max: [f32; 2], pixels_per_point: f32) -> Self {
        let pixel = |value: f32| (value * pixels_per_point).round() as i32;
        Self { left: pixel(min[0]), top: pixel(min[1]), right: pixel(max[0]), bottom: pixel(max[1]) }
    }

    /// Whether the point `(x, y)` is inside (right and bottom edges excluded).
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// Where the UI last said the caption and the maximize button are.
#[derive(Clone, Copy, Debug, Default)]
struct Regions {
    caption: Option<Area>,
    maximize: Option<Area>,
    /// The app's own controls over the caption (the command centre): the
    /// app's, not the caption's.
    holes: [Option<Area>; MOST_HOLES],
}

/// Most controls over the caption [`set_regions_with_holes`] takes.
pub const MOST_HOLES: usize = 4;

/// The regions [`set_regions`] gave, read on each `WM_NCHITTEST`.
static REGIONS: Mutex<Regions> = Mutex::new(Regions { caption: None, maximize: None, holes: [None; MOST_HOLES] });
/// Whether [`install`] took over the caption.
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// The app's window (a Win32 `HWND`), 0 until [`install`] is given it.
static WINDOW: AtomicIsize = AtomicIsize::new(0);
/// The pointer is over the maximize button: the system has it then, and
/// the app hears nothing, so the button's highlight comes from here.
static HOVERED: AtomicBool = AtomicBool::new(false);
/// How to ask the UI to draw again, given to [`install`].
static REPAINT: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Take over `window`'s caption, once; `repaint` asks the UI to draw again
/// (the maximize button's highlight changed). Whether it is taken.
pub fn install(window: &impl HasWindowHandle, repaint: impl Fn() + Send + Sync + 'static) -> bool {
    if INSTALLED.load(Ordering::SeqCst) {
        return true;
    }
    let Ok(handle) = window.window_handle() else { return false };
    let raw_window_handle::RawWindowHandle::Win32(handle) = handle.as_raw() else { return false };
    WINDOW.store(handle.hwnd.get(), Ordering::SeqCst);
    let _ = REPAINT.set(Box::new(repaint));
    #[cfg(windows)]
    {
        let taken = ffi::install(handle.hwnd.get());
        INSTALLED.store(taken, Ordering::SeqCst);
        taken
    }
    #[cfg(not(windows))]
    false
}

/// The program's window as a Win32 `HWND`, once [`install`] has seen it:
/// what a system dialog would be shown over.
pub fn window() -> Option<isize> {
    Some(WINDOW.load(Ordering::SeqCst)).filter(|hwnd| *hwnd != 0)
}

/// Whether the system's frame is kept and asks the app where the caption is.
pub fn installed() -> bool {
    INSTALLED.load(Ordering::SeqCst)
}

/// Where the caption (the menu bar's empty space) and the maximize button
/// are now.
pub fn set_regions(caption: Option<Area>, maximize: Option<Area>) {
    set_regions_with_holes(caption, maximize, &[]);
}

/// [`set_regions`] with the app's own controls that lie over the caption
/// (`holes`, at most [`MOST_HOLES`]; the command centre in the middle of
/// the title bar): those stay the app's.
pub fn set_regions_with_holes(caption: Option<Area>, maximize: Option<Area>, holes: &[Area]) {
    let mut kept = [None; MOST_HOLES];
    for (slot, hole) in kept.iter_mut().zip(holes) {
        *slot = Some(*hole);
    }
    if let Ok(mut regions) = REGIONS.lock() {
        *regions = Regions { caption, maximize, holes: kept };
    }
}

/// Whether the pointer is over the maximize button.
pub fn maximize_hovered() -> bool {
    HOVERED.load(Ordering::SeqCst)
}

/// Note whether the pointer is over the maximize button, and have the UI
/// draw again when that changed.
#[cfg_attr(not(windows), allow(dead_code))]
fn hover(over: bool) {
    if HOVERED.swap(over, Ordering::SeqCst) != over {
        if let Some(repaint) = REPAINT.get() {
            repaint();
        }
    }
}

/// The regions as the UI last set them.
#[cfg_attr(not(windows), allow(dead_code))]
fn regions() -> Regions {
    REGIONS.lock().map(|regions| *regions).unwrap_or_default()
}

/// What a point of the client area is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
enum Hit {
    Client,
    Top,
    TopLeft,
    TopRight,
    Caption,
    Maximize,
}

/// What the point `(x, y)` of a client area `width` wide is, with the
/// system's resize border `border` thick. The left, right and bottom edges
/// are the system's frame; the top one is in the client area, and resizes
/// unless the window is maximized.
#[cfg_attr(not(windows), allow(dead_code))]
fn hit(regions: &Regions, x: i32, y: i32, width: i32, border: i32, maximized: bool) -> Hit {
    if !maximized && y < border {
        return if x < border {
            Hit::TopLeft
        } else if x >= width - border {
            Hit::TopRight
        } else {
            Hit::Top
        };
    }
    if regions.maximize.is_some_and(|area| area.contains(x, y)) {
        Hit::Maximize
    } else if regions.caption.is_some_and(|area| area.contains(x, y))
        && !regions.holes.iter().flatten().any(|hole| hole.contains(x, y))
    {
        Hit::Caption
    } else {
        Hit::Client
    }
}

/// A press on the caption.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
struct Press {
    /// The message time, in milliseconds (it wraps).
    time: u32,
    x: i32,
    y: i32,
}

/// Whether `press` is the second of a double click after `previous`: soon
/// enough (`limit` ms) and near enough (`reach` pixels each way).
#[cfg_attr(not(windows), allow(dead_code))]
fn double_click(previous: Option<Press>, press: Press, limit: u32, reach: (i32, i32)) -> bool {
    previous.is_some_and(|first| {
        press.time.wrapping_sub(first.time) <= limit
            && (press.x - first.x).abs() <= reach.0 / 2
            && (press.y - first.y).abs() <= reach.1 / 2
    })
}

/// The window's messages, through Win32: the only code here that is
/// `unsafe`.
#[cfg(windows)]
#[allow(unsafe_code)]
mod ffi {
    use std::cell::Cell;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetDoubleClickTime, TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
    };
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnableMenuItem, GetClientRect, GetMessageTime, GetSystemMenu, IsZoomed, PostMessageW, SetMenuDefaultItem,
        SetWindowPos, ShowWindow, TrackPopupMenu, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT,
        MF_BYCOMMAND, MF_ENABLED, MF_GRAYED, NCCALCSIZE_PARAMS, SC_CLOSE, SC_MAXIMIZE, SC_MOVE, SC_RESTORE, SC_SIZE,
        SM_CXDOUBLECLK, SM_CXPADDEDBORDER, SM_CYDOUBLECLK, SM_CYFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DPICHANGED, WM_MOUSEMOVE,
        WM_NCCALCSIZE, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE,
        WM_NCMOUSEMOVE, WM_NCRBUTTONUP, WM_SYSCOMMAND,
    };

    use super::{Hit, Press};

    thread_local! {
        /// The last press on the caption, for double clicks.
        static PRESSED: Cell<Option<Press>> = const { Cell::new(None) };
    }

    /// This subclass among the window's.
    const SUBCLASS: usize = 0x5752_4D46;

    /// Subclass the window and have its frame computed again without the
    /// caption; whether the subclass was taken.
    pub(crate) fn install(hwnd: HWND) -> bool {
        // SAFETY: `hwnd` is the app's live window and this runs on its
        // thread (the UI's creator); `subclass` has SUBCLASSPROC's signature.
        let taken = unsafe { SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, 0) } != 0;
        if taken {
            // The frame is computed again at once, without the caption.
            // SAFETY: as above; only the frame changes (no move, size or order).
            unsafe {
                SetWindowPos(
                    hwnd,
                    0,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
        }
        taken
    }

    /// Whether the window is maximized.
    fn maximized(hwnd: HWND) -> bool {
        // SAFETY: a query of the live window.
        unsafe { IsZoomed(hwnd) != 0 }
    }

    /// The resize border's thickness at the window's DPI: what a maximized
    /// window reaches past the screen by.
    fn border(hwnd: HWND) -> i32 {
        // SAFETY: queries of the live window and of the system.
        unsafe {
            let dpi = GetDpiForWindow(hwnd);
            GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
        }
    }

    /// Hand a message on to the window's next procedure.
    fn next(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: the message as it came, to the window's next procedure.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }

    /// The point of a mouse message (signed screen coordinates).
    fn point(lparam: LPARAM) -> POINT {
        POINT { x: i32::from((lparam & 0xFFFF) as u16 as i16), y: i32::from(((lparam >> 16) & 0xFFFF) as u16 as i16) }
    }

    /// Restore a maximized window, maximize any other.
    fn toggle_maximized(hwnd: HWND) {
        let command = if maximized(hwnd) { SW_RESTORE } else { SW_MAXIMIZE };
        // SAFETY: the live window.
        unsafe { ShowWindow(hwnd, command) };
    }

    /// Whether this press on the caption completes a double click. winit
    /// cancels the system's caption loop on each press (a dummy mouse
    /// move, so its event loop does not stall), and the system then misses
    /// the second click: the window counts them itself.
    fn caption_double_click(hwnd: HWND, lparam: LPARAM) -> bool {
        let at = point(lparam);
        // SAFETY: queries of the system and of the current message.
        let (time, limit, reach) = unsafe {
            let dpi = GetDpiForWindow(hwnd);
            (
                GetMessageTime() as u32,
                GetDoubleClickTime(),
                (GetSystemMetricsForDpi(SM_CXDOUBLECLK, dpi), GetSystemMetricsForDpi(SM_CYDOUBLECLK, dpi)),
            )
        };
        let press = Press { time, x: at.x, y: at.y };
        let double = super::double_click(PRESSED.get(), press, limit, reach);
        PRESSED.set(if double { None } else { Some(press) });
        double
    }

    /// The window menu (restore, move, size, minimize, maximize, close)
    /// where the caption was right-clicked, as the system's caption shows it.
    fn window_menu(hwnd: HWND, lparam: LPARAM) {
        let at = point(lparam);
        let zoomed = maximized(hwnd);
        let state = |enabled: bool| MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED };
        // SAFETY: the live window's own menu, shown modally on its thread;
        // the chosen command goes back to the window as the system's would.
        unsafe {
            let menu = GetSystemMenu(hwnd, 0);
            if menu == 0 {
                return;
            }
            EnableMenuItem(menu, SC_RESTORE, state(zoomed));
            EnableMenuItem(menu, SC_MOVE, state(!zoomed));
            EnableMenuItem(menu, SC_SIZE, state(!zoomed));
            EnableMenuItem(menu, SC_MAXIMIZE, state(!zoomed));
            SetMenuDefaultItem(menu, SC_CLOSE, 0);
            let command = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, at.x, at.y, 0, hwnd, std::ptr::null());
            if command != 0 {
                PostMessageW(hwnd, WM_SYSCOMMAND, command as WPARAM, 0);
            }
        }
    }

    /// The caption is the client's (`WM_NCCALCSIZE` with `wparam` true):
    /// keep the system's left, right and bottom edges, and the top where
    /// the window begins (below the screen's top when maximized, past which
    /// the window reaches).
    fn calc_size(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let params = lparam as *mut NCCALCSIZE_PARAMS;
        // SAFETY: with `wparam` true, `lparam` is this call's NCCALCSIZE_PARAMS.
        let top = unsafe { (*params).rgrc[0].top };
        let result = next(hwnd, message, wparam, lparam);
        if result != 0 {
            return result;
        }
        let inset = if maximized(hwnd) { border(hwnd) } else { 0 };
        // SAFETY: as above.
        unsafe { (*params).rgrc[0].top = top + inset };
        0
    }

    /// Onto a screen of another scale (`WM_DPICHANGED`): winit takes the new
    /// scale, then places the window as if it had the system's caption, a
    /// caption taller than the window it gives back and nudged until wholly
    /// on the new screen. Its centre then falls back on the old screen, whose
    /// scale it takes again: the window bounces at the edge and never
    /// crosses. The system's suggested place, as WM_DPICHANGED asks, keeps
    /// the pointer where it is on the window (seagochen/annotary#199).
    fn dpi_changed(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // SAFETY: WM_DPICHANGED's `lparam` is the suggested RECT, valid during the call.
        let suggested = unsafe { *(lparam as *const RECT) };
        let result = next(hwnd, message, wparam, lparam);
        let (width, height) = (suggested.right - suggested.left, suggested.bottom - suggested.top);
        // SAFETY: the live window, moved and sized only.
        unsafe { SetWindowPos(hwnd, 0, suggested.left, suggested.top, width, height, SWP_NOZORDER | SWP_NOACTIVATE) };
        result
    }

    /// What a point of the window is (`WM_NCHITTEST`): the system's answer
    /// unless it says client, where the app's regions decide.
    fn hit_test(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let system = next(hwnd, message, wparam, lparam);
        if system != HTCLIENT as LRESULT {
            return system;
        }
        let mut at = point(lparam);
        let mut client = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        // SAFETY: the live window, and locals that outlive the calls.
        unsafe {
            ScreenToClient(hwnd, &mut at);
            GetClientRect(hwnd, &mut client);
        }
        let code = match super::hit(&super::regions(), at.x, at.y, client.right, border(hwnd), maximized(hwnd)) {
            Hit::Client => HTCLIENT,
            Hit::Top => HTTOP,
            Hit::TopLeft => HTTOPLEFT,
            Hit::TopRight => HTTOPRIGHT,
            Hit::Caption => HTCAPTION,
            Hit::Maximize => HTMAXBUTTON,
        };
        code as LRESULT
    }

    /// The pointer moved over the frame (`WM_NCMOUSEMOVE`): the maximize
    /// button's hover, and hearing when the pointer leaves it for outside
    /// the window.
    fn frame_mouse_move(hwnd: HWND, over_maximize: bool) {
        if over_maximize {
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            // SAFETY: a filled local for the live window.
            unsafe { TrackMouseEvent(&mut track) };
        }
        super::hover(over_maximize);
    }

    /// The window's subclass procedure: the frame without the caption, what
    /// each point is (`WM_NCHITTEST`), double clicks and the window menu on the
    /// caption, the maximize button's hover and the place after a change of
    /// scale; every other message goes on as it came.
    extern "system" fn subclass(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        let maximize = HTMAXBUTTON as WPARAM;
        let caption = HTCAPTION as WPARAM;
        match message {
            WM_NCLBUTTONDOWN if wparam == caption && caption_double_click(hwnd, lparam) => {
                toggle_maximized(hwnd);
                0
            }
            WM_NCLBUTTONDBLCLK if wparam == caption => {
                PRESSED.set(None);
                toggle_maximized(hwnd);
                0
            }
            WM_NCRBUTTONUP if wparam == caption => {
                window_menu(hwnd, lparam);
                0
            }
            WM_NCCALCSIZE if wparam != 0 => calc_size(hwnd, message, wparam, lparam),
            WM_DPICHANGED => dpi_changed(hwnd, message, wparam, lparam),
            WM_NCHITTEST => hit_test(hwnd, message, wparam, lparam),
            // The maximize button is the app's to draw and to press: the
            // system would draw its own old one over it.
            WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam == maximize => 0,
            WM_NCLBUTTONUP if wparam == maximize => {
                toggle_maximized(hwnd);
                0
            }
            WM_NCMOUSEMOVE => {
                frame_mouse_move(hwnd, wparam == maximize);
                next(hwnd, message, wparam, lparam)
            }
            WM_NCMOUSELEAVE | WM_MOUSEMOVE => {
                super::hover(false);
                next(hwnd, message, wparam, lparam)
            }
            _ => next(hwnd, message, wparam, lparam),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_become_rounded_physical_pixels() {
        let area = Area::from_points([10.2, 0.0], [100.5, 35.0], 1.5);
        assert_eq!(area, Area { left: 15, top: 0, right: 151, bottom: 53 });
    }

    #[test]
    fn the_top_edge_resizes_the_caption_moves_and_the_rest_is_the_app_s() {
        let regions = Regions {
            caption: Some(Area { left: 300, top: 0, right: 1500, bottom: 40 }),
            maximize: Some(Area { left: 1600, top: 0, right: 1655, bottom: 40 }),
            holes: [Some(Area { left: 700, top: 5, right: 1000, bottom: 35 }), None, None, None],
        };
        let at = |x, y, maximized| hit(&regions, x, y, 1710, 8, maximized);
        assert_eq!(at(800, 3, false), Hit::Top);
        assert_eq!(at(2, 3, false), Hit::TopLeft);
        assert_eq!(at(1708, 3, false), Hit::TopRight);
        assert_eq!(at(800, 3, true), Hit::Caption, "a maximized window does not resize");
        assert_eq!(at(500, 20, false), Hit::Caption);
        assert_eq!(at(800, 20, false), Hit::Client, "the command centre");
        assert_eq!(at(1620, 20, false), Hit::Maximize);
        assert_eq!(at(100, 20, false), Hit::Client, "the menus");
        assert_eq!(at(800, 300, false), Hit::Client);
        assert_eq!(
            hit(&Regions::default(), 800, 20, 1710, 8, false),
            Hit::Client,
            "before the UI says where the caption is"
        );
    }

    #[test]
    fn a_double_click_is_two_presses_soon_and_near() {
        let press = |time, x, y| Press { time, x, y };
        let first = Some(press(1000, 500, 20));
        assert!(double_click(first, press(1300, 502, 21), 500, (4, 4)));
        assert!(!double_click(first, press(1600, 500, 20), 500, (4, 4)), "too late");
        assert!(!double_click(first, press(1300, 520, 20), 500, (4, 4)), "too far");
        assert!(!double_click(None, press(1300, 500, 20), 500, (4, 4)));
        assert!(
            double_click(Some(press(u32::MAX - 100, 500, 20)), press(100, 500, 20), 500, (4, 4)),
            "the message time wraps"
        );
    }
}
