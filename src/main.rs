#![windows_subsystem = "windows"]

mod config;
mod desktop;
mod drop;
mod editbox;
mod fence;
mod gfx;
mod icons;
mod settings;
mod shell;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::io::Write;
use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Ole::OleInitialize;
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use config::{FenceCfg, Settings};

const HOST_CLASS: PCWSTR = w!("Lightfence.Host");
const WM_TRAY: u32 = WM_APP + 1;
const WM_DESKTOP_CHANGED: u32 = WM_APP + 2;
/// Sent by a second launch: open the settings window.
const WM_SHOW_SETTINGS: u32 = WM_APP + 3;
/// Coalesces bursts of desktop change notifications into one resync.
const SYNC_TIMER: usize = 1;
/// Explorer re-shows its icons on some refreshes; check now and then and hide them again.
const ICON_GUARD_TIMER: usize = 2;

thread_local! {
    static HINST: Cell<HINSTANCE> = Cell::new(HINSTANCE::default());
    static HOST: Cell<HWND> = Cell::new(HWND::default());
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
    static FENCES: RefCell<Vec<HWND>> = const { RefCell::new(Vec::new()) };
    static HIDE_ICONS: Cell<bool> = const { Cell::new(true) };
    static SETTINGS: RefCell<Settings> = RefCell::new(Settings::default());
    /// Where the next new desktop items should go (after "新建", "粘贴" or a drop).
    static EXPECT_NEW: Cell<Option<Expect>> = const { Cell::new(None) };
    /// Desktop items as of the last sync, to tell which ones are new.
    static KNOWN: RefCell<Option<HashSet<String>>> = const { RefCell::new(None) };
    /// The desktop layer (catch-all fence), if icons are hidden.
    static LAYER: Cell<HWND> = Cell::new(HWND::default());
    static WATCH_ID: Cell<u32> = const { Cell::new(0) };
}

pub fn log(msg: &str) {
    let _ = std::fs::create_dir_all(config::dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(config::dir().join("log.txt")) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {msg}");
    }
}

fn main() {
    config::migrate_legacy_dir();
    std::panic::set_hook(Box::new(|info| {
        log(&format!("panic: {info}"));
        // Never leave the user with an empty desktop.
        desktop::set_icons_visible(true);
    }));
    if let Err(e) = unsafe { run() } {
        log(&format!("fatal: {e}"));
    }
    desktop::set_icons_visible(true);
}

unsafe fn run() -> Result<()> {
    let _instance = CreateMutexW(None, true, w!("Local\\Lightfence.SingleInstance"))?;
    if GetLastError() == ERROR_ALREADY_EXISTS {
        // Launching again while running opens the settings.
        if let Ok(host) = FindWindowW(HOST_CLASS, PCWSTR::null()) {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let _ = PostMessageW(Some(host), WM_SHOW_SETTINGS, WPARAM(0), LPARAM(0));
        }
        return Ok(());
    }
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    // OLE (not just COM) so fences can be drag-and-drop targets.
    OleInitialize(None)?;

    let hinst: HINSTANCE = GetModuleHandleW(None)?.into();
    HINST.set(hinst);
    fence::register(hinst)?;
    editbox::register(hinst);
    settings::register(hinst);
    let host = create_host(hinst)?;
    HOST.set(host);
    TASKBAR_CREATED.set(RegisterWindowMessageW(w!("TaskbarCreated")));
    add_tray_icon(host);

    let mut cfg = config::load();
    HIDE_ICONS.set(cfg.hide_desktop_icons);
    SETTINGS.set(cfg.settings.clone());
    if cfg.fences.is_empty() && !cfg.hide_desktop_icons {
        let s = GetDpiForSystem() as f32 / 96.0;
        let p = POINT { x: (220.0 * s) as i32, y: (160.0 * s) as i32 };
        cfg.fences.push(default_fence(p));
    }
    for fc in cfg.fences {
        let desktop = fc.catch_all;
        match fence::create(hinst, fc) {
            Ok(h) => {
                FENCES.with_borrow_mut(|v| v.push(h));
                if desktop {
                    LAYER.set(h);
                }
            }
            Err(e) => log(&format!("create fence failed: {e}")),
        }
    }
    apply_hide_icons();
    WATCH_ID.set(desktop::watch(host, WM_DESKTOP_CHANGED));
    SetTimer(Some(host), ICON_GUARD_TIMER, 1500, None);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    let _ = SHChangeNotifyDeregister(WATCH_ID.get());
    remove_tray_icon(host);
    Ok(())
}

fn default_fence(p: POINT) -> FenceCfg {
    let s = unsafe { GetDpiForSystem() } as f32 / 96.0;
    let id = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    FenceCfg::new(id, "新建分区", p.x, p.y, (380.0 * s) as i32, (280.0 * s) as i32)
}

/// The desktop layer; `fence::create` sizes it to the work area.
fn desktop_fence() -> FenceCfg {
    let mut cfg = default_fence(POINT::default());
    cfg.catch_all = true;
    cfg
}

pub fn save() {
    let fences = FENCES.with_borrow(|v| v.iter().filter_map(|&h| fence::with(h, |f| f.cfg.clone())).collect());
    config::save(&config::Config { fences, settings: settings(), hide_desktop_icons: HIDE_ICONS.get() });
}

#[derive(Clone, Copy)]
struct Expect {
    /// Fence to put them in; `None` means the desktop layer.
    target: Option<HWND>,
    pt: POINT,
    rename: bool,
    at: std::time::Instant,
}

/// Items that appear on the desktop in the next few seconds go into `target` at
/// screen point `pt` (the desktop layer if `None`); with `rename`, the first one
/// starts in rename mode.
pub fn expect_new_items(target: Option<HWND>, pt: POINT, rename: bool) {
    EXPECT_NEW.set(Some(Expect { target, pt, rename, at: std::time::Instant::now() }));
}

pub fn settings() -> Settings {
    SETTINGS.with_borrow(|s| s.clone())
}

/// Changes the settings and applies them to every fence right away.
pub fn update_settings(change: impl FnOnce(&mut Settings)) {
    let old = settings();
    let mut new = old.clone();
    change(&mut new);
    if new == old {
        return;
    }
    SETTINGS.set(new.clone());
    let fences = FENCES.with_borrow(|v| v.clone());
    for h in fences {
        fence::with(h, |f| f.settings_changed(&old, &new));
    }
    save();
}

pub fn hide_icons() -> bool {
    HIDE_ICONS.get()
}

pub fn set_hide_icons(hide: bool) {
    HIDE_ICONS.set(hide);
    apply_hide_icons();
}

fn find_catch_all() -> Option<HWND> {
    desktop_layer()
}

/// The desktop layer window, which must stay below every fence.
pub fn desktop_layer() -> Option<HWND> {
    let h = LAYER.get();
    (!h.is_invalid()).then_some(h)
}

/// Hides or shows Explorer's icons per the setting; while hidden, the catch-all
/// fence exists so no desktop item becomes unreachable.
fn apply_hide_icons() {
    let hide = HIDE_ICONS.get();
    desktop::set_icons_visible(!hide);
    match (hide, find_catch_all()) {
        (true, None) => match unsafe { fence::create(HINST.get(), desktop_fence()) } {
            Ok(h) => {
                FENCES.with_borrow_mut(|v| v.push(h));
                LAYER.set(h);
            }
            Err(e) => log(&format!("create desktop layer failed: {e}")),
        },
        (true, Some(h)) => {
            fence::with(h, |f| f.refit());
        }
        (false, Some(h)) => {
            LAYER.set(HWND::default());
            FENCES.with_borrow_mut(|v| v.retain(|&x| x != h));
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
        _ => {}
    }
    sync_desktop();
}

/// Reconciles fences with what is on the desktop: drops items that are gone and
/// gives every unclaimed item to the catch-all fence.
pub fn sync_desktop() {
    let present = desktop::items();
    let on_desktop: HashSet<String> = present.iter().map(|p| p.to_lowercase()).collect();
    let fresh: Vec<String> = KNOWN.with_borrow(|known| match known {
        Some(k) => present.iter().filter(|p| !k.contains(&p.to_lowercase())).cloned().collect(),
        None => Vec::new(),
    });
    KNOWN.set(Some(on_desktop.clone()));
    let fences = FENCES.with_borrow(|v| v.clone());
    let catch_all = find_catch_all();
    let mut claimed = HashSet::new();
    for &h in &fences {
        fence::with(h, |f| {
            f.retain_items(|p| on_desktop.contains(&p.to_lowercase()));
            if Some(h) != catch_all {
                claimed.extend(f.cfg.all_items().map(|p| p.to_lowercase()));
            }
        });
    }
    if let Some(h) = catch_all {
        let loose: Vec<String> = present.into_iter().filter(|p| !claimed.contains(&p.to_lowercase())).collect();
        let loose_set: HashSet<String> = loose.iter().map(|p| p.to_lowercase()).collect();
        fence::with(h, |f| {
            f.retain_items(|p| loose_set.contains(&p.to_lowercase()));
            f.append_items(&loose);
        });
    }
    if let Some(e) = EXPECT_NEW.get() {
        if e.at.elapsed().as_secs() > 15 {
            EXPECT_NEW.set(None);
        } else if !fresh.is_empty() {
            let alive = |h: &HWND| FENCES.with_borrow(|v| v.contains(h));
            if let Some(target) = e.target.filter(alive).or(catch_all) {
                add_items(target, fresh.clone(), e.pt);
                if e.rename {
                    fence::rename_path(target, &fresh[0]);
                }
            }
            // "新建" makes one item; a copy may arrive in several batches, so keep waiting.
            if e.rename {
                EXPECT_NEW.set(None);
            }
        }
    }
    save();
}

pub fn new_fence_at(p: POINT) {
    match unsafe { fence::create(HINST.get(), default_fence(p)) } {
        Ok(h) => {
            FENCES.with_borrow_mut(|v| v.push(h));
            save();
        }
        Err(e) => log(&format!("create fence failed: {e}")),
    }
}

/// Puts desktop items dropped at screen point `pt` into `target`; an item lives
/// in at most one fence (or on the desktop).
pub fn add_items(target: HWND, paths: Vec<String>, pt: POINT) {
    let fences = FENCES.with_borrow(|v| v.clone());
    for h in fences {
        fence::with(h, |f| {
            if h == target {
                f.accept_drop(&paths, pt);
            } else {
                f.remove_items(&paths);
            }
        });
    }
    save();
}

/// Screen rectangles of all fences except `except`, for edge snapping.
pub fn fence_rects(except: HWND) -> Vec<RECT> {
    FENCES.with_borrow(|v| {
        v.iter()
            .filter(|&&h| h != except && Some(h) != desktop_layer())
            .filter_map(|&h| {
                let mut r = RECT::default();
                unsafe { GetWindowRect(h, &mut r) }.ok().map(|_| r)
            })
            .collect()
    })
}

pub fn remove_fence(hwnd: HWND) {
    FENCES.with_borrow_mut(|v| v.retain(|&h| h != hwnd));
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    // Its items go back to the catch-all fence.
    sync_desktop();
}

/// Makes `hwnd` the foreground window so an open menu closes when the user clicks
/// elsewhere. Fences never activate, so borrow the input state of whoever is in front.
pub fn take_foreground(hwnd: HWND) {
    unsafe {
        let me = GetCurrentThreadId();
        let front = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let attached = front != 0 && front != me && AttachThreadInput(me, front, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(me, front, false);
        }
    }
}

/// Hidden window that owns popup menus, so fences never get activated.
pub fn host() -> HWND {
    HOST.get()
}

pub fn quit() {
    save();
    desktop::set_icons_visible(true);
    unsafe { PostQuitMessage(0) };
}

/// Shows a popup menu at `p` and returns the chosen id (0 if dismissed).
/// `(0, _)` entries become separators. Owned by the hidden host window so
/// fences never get activated.
pub fn popup_menu(p: POINT, items: &[(u32, &str)]) -> u32 {
    let host = HOST.get();
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        for &(id, text) in items {
            let _ = if id == 0 {
                AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null())
            } else {
                AppendMenuW(menu, MF_STRING, id as usize, &HSTRING::from(text))
            };
        }
        take_foreground(host);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, p.x, p.y, None, host, None).0 as u32;
        let _ = PostMessageW(Some(host), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd
    }
}

unsafe fn create_host(hinst: HINSTANCE) -> Result<HWND> {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(host_proc),
        hInstance: hinst,
        lpszClassName: HOST_CLASS,
        ..Default::default()
    };
    RegisterClassExW(&wc);
    // A real (hidden) top-level window: message-only windows miss "TaskbarCreated".
    CreateWindowExW(WS_EX_TOOLWINDOW, HOST_CLASS, w!("Lightfence"), WS_POPUP, 0, 0, 0, 0, None, None, Some(hinst), None)
}

/// The app icon (resource 1) at `size` pixels, loaded once per size.
pub fn app_icon(size: i32) -> HICON {
    thread_local! {
        static ICONS: RefCell<Vec<(i32, HICON)>> = const { RefCell::new(Vec::new()) };
    }
    ICONS.with_borrow_mut(|cache| {
        if let Some(&(_, icon)) = cache.iter().find(|(s, _)| *s == size) {
            return icon;
        }
        let res = PCWSTR(1 as *const u16); // MAKEINTRESOURCE(1)
        let icon = unsafe { LoadImageW(Some(HINST.get()), res, IMAGE_ICON, size, size, LR_DEFAULTCOLOR) }
            .map(|h| HICON(h.0))
            .unwrap_or_default();
        cache.push((size, icon));
        icon
    })
}

fn tray_data(host: HWND) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: host,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        hIcon: app_icon(unsafe { GetSystemMetrics(SM_CXSMICON) }),
        ..Default::default()
    };
    for (dst, src) in nid.szTip.iter_mut().zip("Lightfence".encode_utf16()) {
        *dst = src;
    }
    nid
}

unsafe fn add_tray_icon(host: HWND) {
    let _ = Shell_NotifyIconW(NIM_ADD, &tray_data(host));
}

unsafe fn remove_tray_icon(host: HWND) {
    let _ = Shell_NotifyIconW(NIM_DELETE, &tray_data(host));
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(r) = shell::forward_menu_msg(msg, wp, lp) {
        return r;
    }
    if msg == WM_TRAY {
        let event = (lp.0 & 0xFFFF) as u32;
        if event == WM_RBUTTONUP || event == WM_LBUTTONUP {
            let mut p = POINT::default();
            let _ = GetCursorPos(&mut p);
            let toggle = if HIDE_ICONS.get() { "显示系统桌面图标" } else { "隐藏系统桌面图标" };
            match popup_menu(p, &[(1, "新建分区"), (2, toggle), (3, "设置…"), (0, ""), (9, "退出")]) {
                1 => new_fence_at(p),
                2 => set_hide_icons(!HIDE_ICONS.get()),
                3 => settings::show(),
                9 => quit(),
                _ => {}
            }
        }
        return LRESULT(0);
    }
    if msg == WM_SHOW_SETTINGS {
        settings::show();
        return LRESULT(0);
    }
    if msg == WM_DESKTOP_CHANGED {
        if let Some((old, new)) = desktop::renamed(wp, lp) {
            // Keep a renamed item in its fence instead of letting it fall back to 未分类.
            FENCES.with_borrow(|v| v.iter().any(|&h| fence::with(h, |f| f.rename_item(&old, &new)).unwrap_or(false)));
        }
        SetTimer(Some(hwnd), SYNC_TIMER, 300, None);
        return LRESULT(0);
    }
    if msg == WM_TIMER && wp.0 == ICON_GUARD_TIMER {
        if HIDE_ICONS.get() && desktop::icons_visible() {
            desktop::set_icons_visible(false);
        }
        return LRESULT(0);
    }
    if msg == WM_TIMER && wp.0 == SYNC_TIMER {
        let _ = KillTimer(Some(hwnd), SYNC_TIMER);
        sync_desktop();
        return LRESULT(0);
    }
    if msg == WM_CLOSE {
        // A polite close (e.g. taskkill without /F) still restores the desktop.
        quit();
        return LRESULT(0);
    }
    if msg == WM_DISPLAYCHANGE || msg == WM_SETTINGCHANGE {
        // Explorer may re-show its icons when the screen layout or desktop icon settings change.
        apply_hide_icons();
    }
    if msg == TASKBAR_CREATED.get() && msg != 0 {
        // Explorer restarted: re-add the tray icon and re-attach fences to the new desktop.
        add_tray_icon(hwnd);
        if let Some(owner) = desktop::host() {
            FENCES.with_borrow(|v| v.iter().for_each(|&h| fence::set_owner(h, owner)));
        }
        let _ = SHChangeNotifyDeregister(WATCH_ID.get());
        WATCH_ID.set(desktop::watch(hwnd, WM_DESKTOP_CHANGED));
        apply_hide_icons();
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
