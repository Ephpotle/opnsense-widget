//! Dragging between monitors with different scaling (150 % 4K <-> 100 % 1080p).
//!
//! Windows expects WM_DPICHANGED to be answered with exactly the rectangle it suggests
//! (lParam); its drag loop is built around that rectangle. winit computes its own instead
//! (same logical size, re-anchored on the cursor), and near the screen edge that left the
//! window half on each screen, so the DPI flipped back and nested changes compounded the size
//! (seen: 899 -> 1347 -> ... -> 4550 px, and 1349 -> 901 -> ... -> 122 px).
//! VLC hit the same loop and fixed it the same way: use the suggested rect unmodified.
//!
//! winit still has to handle the message (it updates its scale factor, which egui renders
//! with), so the subclass lets it run and rewrites winit's own SetWindowPos, in
//! WM_WINDOWPOSCHANGING, to the suggested rectangle: one move, no intermediate position.

use std::cell::Cell;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GUI_INMOVESIZE, GUITHREADINFO, GetGUIThreadInfo, GetWindowThreadProcessId, SWP_NOMOVE, SWP_NOSIZE, WINDOWPOS,
    WM_DPICHANGED, WM_WINDOWPOSCHANGING,
};

thread_local! {
    /// Set while winit handles a WM_DPICHANGED: the rectangle Windows suggested.
    static SUGGESTED: Cell<Option<RECT>> = const { Cell::new(None) };
}

pub fn install(hwnd: HWND) {
    unsafe { SetWindowSubclass(hwnd, Some(subclass), 1, 0) };
}

/// Install on one of this process's own windows, found by title (for egui viewports, whose
/// HWND egui does not expose). Installing twice is harmless: same callback and id.
pub fn install_by_title(title: &str) -> bool {
    own_window(title).map(install).is_some()
}

pub fn hide_by_title(title: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, ShowWindow};
    if let Some(h) = own_window(title) {
        unsafe { ShowWindow(h, SW_HIDE) };
    }
}

/// This process's visible top-level window with the given title. Enumerated rather than
/// FindWindow'd: "Router" is a common title, and test instances run alongside the real one.
fn own_window(title: &str) -> Option<HWND> {
    use windows_sys::Win32::Foundation::{BOOL, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};
    struct Search {
        title: Vec<u16>,
        found: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, l: LPARAM) -> BOOL {
        let search = unsafe { &mut *(l as *mut Search) };
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
        if pid != std::process::id() || unsafe { IsWindowVisible(hwnd) } == 0 {
            return 1;
        }
        let mut buf = [0u16; 128];
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) } as usize;
        if buf[..n] == search.title[..] {
            search.found = hwnd;
            return 0;
        }
        1
    }
    let mut search = Search { title: title.encode_utf16().collect(), found: std::ptr::null_mut() };
    unsafe { EnumWindows(Some(visit), &mut search as *mut Search as LPARAM) };
    (!search.found.is_null()).then_some(search.found)
}

/// Put window `child` next to `parent` on the parent's monitor: to its right if it fits,
/// else to its left, else overlapping; always inside that monitor's work area.
/// Returns false if either window does not exist yet.
pub fn place_beside(child: &str, parent: &str) -> bool {
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowRect, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos};
    let (Some(c), Some(p)) = (own_window(child), own_window(parent)) else { return false };
    let zero = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    let (mut pr, mut cr) = (zero, zero);
    unsafe {
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetWindowRect(p, &mut pr) == 0 || GetMonitorInfoW(MonitorFromWindow(p, MONITOR_DEFAULTTONEAREST), &mut mi) == 0 {
            return false;
        }
        // Move onto the parent's monitor first: a DPI change there rescales the child.
        SetWindowPos(c, std::ptr::null_mut(), pr.left + 40, pr.top + 40, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
        if GetWindowRect(c, &mut cr) == 0 {
            return false;
        }
        let (w, h) = (cr.right - cr.left, cr.bottom - cr.top);
        let work = mi.rcWork;
        let x = if pr.right + w <= work.right {
            pr.right
        } else if pr.left - w >= work.left {
            pr.left - w
        } else {
            (pr.left + 40).clamp(work.left, (work.right - w).max(work.left))
        };
        let y = pr.top.clamp(work.top, (work.bottom - h).max(work.top));
        SetWindowPos(c, std::ptr::null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
    }
    true
}

/// True while the user is dragging or resizing the window (Windows' modal move/size loop).
/// The mouse-button state is not reliable for this: it read "up" during real drags.
pub fn in_move_size(hwnd: HWND) -> bool {
    unsafe {
        let tid = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let mut info: GUITHREADINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        GetGUIThreadInfo(tid, &mut info) != 0 && info.flags & GUI_INMOVESIZE != 0
    }
}

/// Rewrite a pending SetWindowPos so it lands on `r`.
fn steer(pos: &mut WINDOWPOS, r: RECT) {
    pos.x = r.left;
    pos.y = r.top;
    pos.cx = r.right - r.left;
    pos.cy = r.bottom - r.top;
    pos.flags &= !(SWP_NOMOVE | SWP_NOSIZE);
}

unsafe extern "system" fn subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    match msg {
        WM_DPICHANGED if lparam != 0 => {
            let suggested = unsafe { *(lparam as *const RECT) };
            let outer = SUGGESTED.with(|s| s.replace(Some(suggested)));
            let result = unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) };
            SUGGESTED.with(|s| s.set(outer));
            result
        }
        WM_WINDOWPOSCHANGING if lparam != 0 => {
            if let Some(r) = SUGGESTED.with(|s| s.get()) {
                steer(unsafe { &mut *(lparam as *mut WINDOWPOS) }, r);
            }
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steers_window_pos_to_the_suggested_rect() {
        let mut pos: WINDOWPOS = unsafe { std::mem::zeroed() };
        pos.flags = SWP_NOMOVE;
        steer(&mut pos, RECT { left: -450, top: 501, right: 451, bottom: 1046 });
        assert_eq!((pos.x, pos.y, pos.cx, pos.cy), (-450, 501, 901, 545));
        assert_eq!(pos.flags & SWP_NOMOVE, 0);
    }
}
