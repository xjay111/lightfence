//! A small text box that pops up over a fence for renaming a tab or typing a
//! search. Fences are layered windows that never take focus, so the box is its
//! own top-level window around a standard EDIT control, which also gives us the
//! system IME for Chinese input.

use std::cell::RefCell;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::*;

const CLASS: PCWSTR = w!("Lightfence.EditBox");
// Edit control bits that live in Win32_UI_Controls in the windows crate.
const ES_AUTOHSCROLL: u32 = 0x0080;
const EM_SETSEL: u32 = 0x00B1;
const EN_CHANGE: u32 = 0x0300;
const EN_KILLFOCUS: u32 = 0x0200;
const WM_CLOSE_EDITOR: u32 = WM_APP + 20;

#[derive(Clone, PartialEq)]
pub enum Purpose {
    RenameTab(usize),
    /// Renaming the desktop item at this path.
    RenameItem(String),
    Search,
}

struct Editor {
    popup: HWND,
    edit: HWND,
    fence: HWND,
    purpose: Purpose,
    font: HFONT,
    brush: HBRUSH,
}

thread_local! {
    static EDITOR: RefCell<Option<Editor>> = const { RefCell::new(None) };
}

pub unsafe fn register(hinst: HINSTANCE) {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(popup_proc),
        hInstance: hinst,
        lpszClassName: CLASS,
        ..Default::default()
    };
    RegisterClassExW(&wc);
}

/// Opens the box over `rect` (screen pixels) for `fence`, pre-filled with `text`
/// and its first `select` UTF-16 units selected (all of it if `None`).
pub fn open(fence: HWND, purpose: Purpose, rect: RECT, text: &str, select: Option<usize>, dpi: u32) {
    close(false);
    unsafe {
        let hinst = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).map(HINSTANCE::from).unwrap_or_default();
        let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
        let Ok(popup) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            PCWSTR::null(),
            WS_POPUP | WS_BORDER,
            rect.left,
            rect.top,
            w,
            h,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            return;
        };
        let pad = (4 * dpi / 96) as i32;
        let Ok(edit) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("EDIT"),
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_AUTOHSCROLL),
            pad,
            pad,
            w - 2 * pad - 2,
            h - 2 * pad - 2,
            Some(popup),
            None,
            Some(hinst),
            None,
        ) else {
            let _ = DestroyWindow(popup);
            return;
        };
        let font = CreateFontW(
            -((13.5 * dpi as f32 / 96.0) as i32),
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Microsoft YaHei UI"),
        );
        SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        let _ = SetWindowSubclass(edit, Some(edit_proc), 1, 0);
        let brush = CreateSolidBrush(COLORREF(0x002C2826));
        EDITOR.set(Some(Editor { popup, edit, fence, purpose, font, brush }));

        let _ = ShowWindow(popup, SW_SHOW);
        crate::take_foreground(popup);
        let _ = SetFocus(Some(edit));
        let end = select.map(|n| n as isize).unwrap_or(-1);
        SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(end)));
    }
}

/// The box currently open for `fence`, if any.
pub fn purpose_for(fence: HWND) -> Option<Purpose> {
    EDITOR.with_borrow(|e| e.as_ref().filter(|e| e.fence == fence).map(|e| e.purpose.clone()))
}

fn text(edit: HWND) -> String {
    unsafe {
        let len = GetWindowTextLengthW(edit) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(edit, &mut buf) as usize;
        String::from_utf16_lossy(&buf[..n])
    }
}

/// Closes the box. `commit` applies a rename; a search keeps its query either way.
fn close(commit: bool) {
    let Some(e) = EDITOR.take() else { return };
    let value = text(e.edit);
    unsafe {
        let _ = DestroyWindow(e.popup);
        let _ = DeleteObject(e.font.into());
        let _ = DeleteObject(e.brush.into());
    }
    match e.purpose {
        Purpose::RenameTab(i) if commit => crate::fence::rename_tab(e.fence, i, &value),
        Purpose::RenameItem(path) if commit && !value.trim().is_empty() => {
            crate::shell::rename(e.fence, &path, value.trim());
            crate::fence::editor_closed(e.fence);
        }
        _ => crate::fence::editor_closed(e.fence),
    }
}

unsafe extern "system" fn edit_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _: usize, _: usize) -> LRESULT {
    if msg == WM_KEYDOWN && (wp.0 == VK_RETURN.0 as usize || wp.0 == VK_ESCAPE.0 as usize) {
        let enter = wp.0 == VK_RETURN.0 as usize;
        let purpose = EDITOR.with_borrow(|e| e.as_ref().map(|e| (e.purpose.clone(), e.fence)));
        if let Some((Purpose::Search, fence)) = purpose {
            if !enter {
                crate::fence::set_search(fence, "");
            }
        }
        close(enter);
        return LRESULT(0);
    }
    if msg == WM_CHAR && (wp.0 == 0x0D || wp.0 == 0x1B) {
        return LRESULT(0); // no beep for Enter/Esc
    }
    DefSubclassProc(hwnd, msg, wp, lp)
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let code = ((wp.0 >> 16) & 0xFFFF) as u32;
            let state = EDITOR.with_borrow(|e| e.as_ref().map(|e| (e.purpose.clone(), e.fence, e.edit)));
            match (code, state) {
                (EN_CHANGE, Some((Purpose::Search, fence, edit))) => crate::fence::set_search(fence, &text(edit)),
                // Clicking elsewhere commits; close after the focus change has finished.
                (EN_KILLFOCUS, Some(_)) => {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE_EDITOR, WPARAM(0), LPARAM(0));
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_CLOSE_EDITOR => {
            if EDITOR.with_borrow(|e| e.as_ref().is_some_and(|e| e.popup == hwnd)) {
                close(true);
            }
            LRESULT(0)
        }
        WM_CTLCOLOREDIT => {
            let dc = HDC(wp.0 as _);
            SetTextColor(dc, COLORREF(0x00F0F0F0));
            SetBkColor(dc, COLORREF(0x002C2826));
            LRESULT(EDITOR.with_borrow(|e| e.as_ref().map(|e| e.brush.0 as isize).unwrap_or(0)))
        }
        WM_ERASEBKGND => {
            let mut r = RECT::default();
            let _ = GetClientRect(hwnd, &mut r);
            if let Some(brush) = EDITOR.with_borrow(|e| e.as_ref().map(|e| e.brush)) {
                FillRect(HDC(wp.0 as _), &r, brush);
            }
            LRESULT(1)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
