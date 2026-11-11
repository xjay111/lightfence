//! The settings window: fence colour and opacity, icon size, auto-hide, desktop
//! icons and start-with-Windows. Changes apply live.

use std::cell::Cell;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Controls::Dialogs::*;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::Settings;

const CLASS: PCWSTR = w!("Lightfence.Settings");
const TBM_GETPOS: u32 = WM_USER;
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("Lightfence");

const ID_COLOR: i32 = 101;
const ID_OPACITY: i32 = 102;
const ID_ICON: i32 = 103;
const ID_AUTOHIDE: i32 = 104;
const ID_DELAY: i32 = 105;
const ID_HIDEICONS: i32 = 106;
const ID_AUTOSTART: i32 = 107;
const ID_DEFAULTS: i32 = 108;
const ID_DONE: i32 = 109;
const ID_OPACITY_VALUE: i32 = 202;
const ID_ICON_VALUE: i32 = 203;
const ID_DELAY_VALUE: i32 = 205;

thread_local! {
    static WINDOW: Cell<HWND> = Cell::new(HWND::default());
    static FONTS: Cell<(HFONT, HFONT)> = Cell::new((HFONT::default(), HFONT::default()));
    /// The colour dialog's 16 custom colour slots, kept for the session.
    static CUSTOM_COLORS: Cell<[COLORREF; 16]> = Cell::new([COLORREF(0xFFFFFF); 16]);
}

pub unsafe fn register(hinst: HINSTANCE) {
    let icc = INITCOMMONCONTROLSEX { dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_BAR_CLASSES };
    let _ = InitCommonControlsEx(&icc);
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(proc),
        hInstance: hinst,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: GetSysColorBrush(COLOR_WINDOW),
        hIcon: crate::app_icon(32),
        hIconSm: crate::app_icon(16),
        lpszClassName: CLASS,
        ..Default::default()
    };
    RegisterClassExW(&wc);
}

/// Opens the settings window, or brings it forward if it is already open.
pub fn show() {
    unsafe {
        let existing = WINDOW.get();
        if !existing.is_invalid() && IsWindow(Some(existing)).as_bool() {
            let _ = ShowWindow(existing, SW_RESTORE);
            crate::take_foreground(existing);
            return;
        }
        let hinst: HINSTANCE = GetModuleHandleW(None).map(Into::into).unwrap_or_default();
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("Lightfence 设置"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            100,
            100,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            return;
        };
        WINDOW.set(hwnd);
        build(hwnd, hinst);
        let _ = ShowWindow(hwnd, SW_SHOW);
        crate::take_foreground(hwnd);
    }
}

/// Lays out the controls for the window's DPI and sizes the window around them.
unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let px = |v: i32| v * dpi / 96;
    let font = |size: i32, weight: i32| {
        CreateFontW(-px(size), 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, 0, w!("Microsoft YaHei UI"))
    };
    let (normal, bold) = (font(14, FW_NORMAL.0 as i32), font(15, FW_SEMIBOLD.0 as i32));
    FONTS.set((normal, bold));

    let add = |class: PCWSTR, text: &str, style: u32, id: i32, x: i32, y: i32, w: i32, h: i32, f: HFONT| {
        let ctl = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(style),
            px(x),
            px(y),
            px(w),
            px(h),
            Some(hwnd),
            Some(HMENU(id as isize as _)),
            Some(hinst),
            None,
        )
        .unwrap_or_default();
        SendMessageW(ctl, WM_SETFONT, Some(WPARAM(f.0 as usize)), Some(LPARAM(1)));
        ctl
    };
    let tab = WS_TABSTOP.0;
    let slider = |id: i32, y: i32, min: i32, max: i32, pos: i32| {
        let tb = add(w!("msctls_trackbar32"), "", tab | TBS_NOTICKS, id, 130, y - 3, 220, 30, normal);
        SendMessageW(tb, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(((max << 16) | min) as isize)));
        SendMessageW(tb, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(pos as isize)));
    };
    let s = crate::settings();

    add(w!("STATIC"), "分区外观", 0, -1, 24, 18, 300, 22, bold);
    add(w!("STATIC"), "背景颜色", 0, -1, 32, 54, 90, 22, normal);
    add(w!("BUTTON"), "", tab | BS_OWNERDRAW as u32, ID_COLOR, 136, 50, 64, 28, normal);
    add(w!("STATIC"), "不透明度", 0, -1, 32, 94, 90, 22, normal);
    slider(ID_OPACITY, 94, 0, 100, s.opacity as i32);
    add(w!("STATIC"), &format!("{}%", s.opacity), 0, ID_OPACITY_VALUE, 356, 94, 70, 22, normal);
    add(w!("STATIC"), "图标大小", 0, -1, 32, 134, 90, 22, normal);
    slider(ID_ICON, 134, 32, 72, s.icon_size as i32);
    add(w!("STATIC"), &format!("{} 像素", s.icon_size), 0, ID_ICON_VALUE, 356, 134, 70, 22, normal);

    add(w!("STATIC"), "自动收起", 0, -1, 24, 184, 300, 22, bold);
    let auto_hide = add(w!("BUTTON"), "贴着屏幕边缘的分区，鼠标离开后收起到标题栏", tab | BS_AUTOCHECKBOX as u32, ID_AUTOHIDE, 32, 216, 390, 24, normal);
    set_check(auto_hide, s.auto_hide);
    add(w!("STATIC"), "收起延迟", 0, -1, 32, 254, 90, 22, normal);
    slider(ID_DELAY, 254, 0, 30, (s.auto_hide_delay_ms / 100) as i32);
    add(w!("STATIC"), &delay_text(s.auto_hide_delay_ms), 0, ID_DELAY_VALUE, 356, 254, 70, 22, normal);

    add(w!("STATIC"), "其他", 0, -1, 24, 304, 300, 22, bold);
    let hide_icons = add(w!("BUTTON"), "隐藏系统桌面图标（由 Lightfence 显示）", tab | BS_AUTOCHECKBOX as u32, ID_HIDEICONS, 32, 336, 390, 24, normal);
    set_check(hide_icons, crate::hide_icons());
    let autostart = add(w!("BUTTON"), "开机自动启动", tab | BS_AUTOCHECKBOX as u32, ID_AUTOSTART, 32, 366, 390, 24, normal);
    set_check(autostart, autostart_enabled());

    add(w!("BUTTON"), "恢复默认", tab, ID_DEFAULTS, 24, 418, 100, 32, normal);
    add(w!("BUTTON"), "完成", tab | BS_DEFPUSHBUTTON as u32, ID_DONE, 330, 418, 100, 32, normal);

    // Size the window so the client area is 454 x 474 DIPs.
    let mut r = RECT { left: 0, top: 0, right: px(454), bottom: px(474) };
    let _ = AdjustWindowRectExForDpi(&mut r, WINDOW_STYLE(GetWindowLongW(hwnd, GWL_STYLE) as u32), false, WINDOW_EX_STYLE(0), dpi as u32);
    let _ = SetWindowPos(hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
}

fn delay_text(ms: u32) -> String {
    format!("{:.1} 秒", ms as f32 / 1000.0)
}

unsafe fn set_check(ctl: HWND, on: bool) {
    SendMessageW(ctl, BM_SETCHECK, Some(WPARAM(if on { BST_CHECKED.0 as usize } else { 0 })), None);
}

unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    GetDlgItem(Some(hwnd), id).is_ok_and(|c| SendMessageW(c, BM_GETCHECK, None, None).0 == BST_CHECKED.0 as isize)
}

unsafe fn set_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(c) = GetDlgItem(Some(hwnd), id) {
        let _ = SetWindowTextW(c, &HSTRING::from(text));
    }
}

unsafe fn slider_pos(ctl: HWND) -> i32 {
    SendMessageW(ctl, TBM_GETPOS, None, None).0 as i32
}

fn autostart_enabled() -> bool {
    let mut size = 0u32;
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)) }.is_ok()
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, REG_SZ.0, Some(value.as_ptr() as _), (value.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

fn colorref([r, g, b]: [u8; 3]) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

unsafe fn pick_color(hwnd: HWND) {
    let mut custom = CUSTOM_COLORS.get();
    let mut cc = CHOOSECOLORW {
        lStructSize: size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: hwnd,
        rgbResult: colorref(crate::settings().color),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        ..Default::default()
    };
    if ChooseColorW(&mut cc).as_bool() {
        let c = cc.rgbResult.0;
        crate::update_settings(|s| s.color = [c as u8, (c >> 8) as u8, (c >> 16) as u8]);
        if let Ok(btn) = GetDlgItem(Some(hwnd), ID_COLOR) {
            let _ = InvalidateRect(Some(btn), None, true);
        }
    }
    CUSTOM_COLORS.set(custom);
}

/// Puts every control back in sync with the current settings (after "恢复默认").
unsafe fn reload(hwnd: HWND) {
    let s = crate::settings();
    for (id, pos) in [(ID_OPACITY, s.opacity as i32), (ID_ICON, s.icon_size as i32), (ID_DELAY, (s.auto_hide_delay_ms / 100) as i32)] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            SendMessageW(c, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(pos as isize)));
        }
    }
    set_text(hwnd, ID_OPACITY_VALUE, &format!("{}%", s.opacity));
    set_text(hwnd, ID_ICON_VALUE, &format!("{} 像素", s.icon_size));
    set_text(hwnd, ID_DELAY_VALUE, &delay_text(s.auto_hide_delay_ms));
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_AUTOHIDE) {
        set_check(c, s.auto_hide);
    }
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_COLOR) {
        let _ = InvalidateRect(Some(c), None, true);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_HSCROLL => {
            let ctl = HWND(lp.0 as _);
            let code = (wp.0 & 0xFFFF) as u32;
            let pos = slider_pos(ctl);
            match GetDlgCtrlID(ctl) {
                ID_OPACITY => {
                    set_text(hwnd, ID_OPACITY_VALUE, &format!("{pos}%"));
                    crate::update_settings(|s| s.opacity = pos as u8);
                }
                ID_ICON => {
                    let size = (pos + 2) / 4 * 4; // steps of 4 px
                    set_text(hwnd, ID_ICON_VALUE, &format!("{size} 像素"));
                    // Reloading every icon is slow, so only apply once the slider is released.
                    if code == TB_ENDTRACK {
                        SendMessageW(ctl, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(size as isize)));
                        crate::update_settings(|s| s.icon_size = size as u32);
                    }
                }
                ID_DELAY => {
                    set_text(hwnd, ID_DELAY_VALUE, &delay_text(pos as u32 * 100));
                    crate::update_settings(|s| s.auto_hide_delay_ms = pos as u32 * 100);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match (wp.0 & 0xFFFF) as i32 {
                ID_COLOR => pick_color(hwnd),
                ID_AUTOHIDE => {
                    let on = checked(hwnd, ID_AUTOHIDE);
                    crate::update_settings(|s| s.auto_hide = on);
                }
                ID_HIDEICONS => crate::set_hide_icons(checked(hwnd, ID_HIDEICONS)),
                ID_AUTOSTART => set_autostart(checked(hwnd, ID_AUTOSTART)),
                ID_DEFAULTS => {
                    crate::update_settings(|s| *s = Settings::default());
                    reload(hwnd);
                }
                ID_DONE => {
                    let _ = DestroyWindow(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            // The colour swatch button.
            let di = &*(lp.0 as *const DRAWITEMSTRUCT);
            let frame = CreateSolidBrush(COLORREF(0x00A0A0A0));
            let fill = CreateSolidBrush(colorref(crate::settings().color));
            let mut r = di.rcItem;
            FillRect(di.hDC, &r, frame);
            r.left += 2;
            r.top += 2;
            r.right -= 2;
            r.bottom -= 2;
            FillRect(di.hDC, &r, fill);
            if di.itemState.0 & ODS_FOCUS.0 != 0 {
                let _ = DrawFocusRect(di.hDC, &di.rcItem);
            }
            let _ = DeleteObject(frame.into());
            let _ = DeleteObject(fill.into());
            LRESULT(1)
        }
        WM_CTLCOLORSTATIC => {
            // Labels on the window's own (white) background.
            let dc = HDC(wp.0 as _);
            SetBkMode(dc, TRANSPARENT);
            LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize)
        }
        WM_DESTROY => {
            let (a, b) = FONTS.get();
            let _ = DeleteObject(a.into());
            let _ = DeleteObject(b.into());
            WINDOW.set(HWND::default());
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
