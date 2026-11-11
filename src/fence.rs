//! A fence: a translucent layered window on the desktop layer that shows a grid of
//! desktop items and can be moved, resized (with edge snapping) and collapsed.
//!
//! The catch-all fence (`cfg.catch_all`) is the desktop itself: a borderless,
//! transparent window over the work area where loose items sit on a free grid.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use windows::core::{w, Result, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::{IDWriteTextFormat, DWRITE_MEASURING_MODE_NATURAL};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, ScreenToClient, MONITORINFO, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::Ole::{RegisterDragDrop, RevokeDragDrop};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_numerics::Vector2;

use crate::config::{FenceCfg, Settings, Tab};
use crate::editbox::{self, Purpose};
use crate::gfx::{self, rgba, Surface};
use crate::icons;

const CLASS: PCWSTR = w!("Lightfence.Fence");
// Lives in Win32_UI_Controls in the windows crate; not worth the whole feature.
const WM_MOUSELEAVE: u32 = 0x02A3;

// Resize edge flags.
const L: u8 = 1;
const T: u8 = 2;
const R: u8 = 4;
const B: u8 = 8;

#[derive(Clone, Copy, PartialEq)]
enum Zone {
    Body,
    Title,
    Chevron,
    Resize(u8),
    Tab(usize),
    AddTab,
    Search,
    ClearSearch,
}

/// Auto-hide: wait this long after the mouse leaves, then shrink.
const HIDE_TIMER: usize = 10;
/// Drives the shrink/expand animation.
const ANIM_TIMER: usize = 11;
const ANIM_MS: f32 = 160.0;

/// The screen edge a fence sits against.
#[derive(Clone, Copy, PartialEq)]
enum Dock {
    Top,
    Bottom,
    Side,
}

/// Height animation between shrunk and expanded.
#[derive(Clone, Copy)]
struct Anim {
    from: f32,
    to: f32,
    start: std::time::Instant,
}

/// Title bar layout: tab labels, the "+" button and the search button.
struct Bar {
    tabs: Vec<D2D_RECT_F>,
    plus: D2D_RECT_F,
    search: D2D_RECT_F,
}

fn contains(r: &D2D_RECT_F, x: i32, y: i32) -> bool {
    (x as f32) >= r.left && (x as f32) < r.right && (y as f32) >= r.top && (y as f32) < r.bottom
}

/// Moving or resizing the fence window itself.
#[derive(Clone, Copy)]
struct Drag {
    zone: Zone,
    cursor: POINT,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    moved: bool,
}

/// Left button held on an item; becomes a shell drag once the mouse moves far enough.
#[derive(Clone, Copy)]
struct Press {
    item: usize,
    at: POINT,
    /// The item was already selected: a plain click (no drag) narrows the
    /// selection to it on release, so a multi-selection can still be dragged.
    narrow_on_release: bool,
}

/// Rubber-band selection in client coordinates.
struct Band {
    start: POINT,
    cur: POINT,
    /// Selection when the band started (kept when Ctrl is held).
    base: HashSet<String>,
}

/// Drop feedback while something is dragged over the fence.
#[derive(Clone, Copy, PartialEq)]
enum Marker {
    /// Insertion bar before index (or after the previous cell at a row end).
    Bar(usize, bool),
    /// Target cell on the desktop grid.
    Cell(i32, i32),
}

struct Item {
    path: String,
    key: String,
    name: Vec<u16>,
    /// Lowercased display name, for search.
    name_lower: String,
    pixels: Option<icons::Pixels>,
    bmp: Option<ID2D1Bitmap>,
    /// Grid cell (column, row); only used on the desktop.
    cell: (i32, i32),
}

impl Item {
    fn load(path: &str, icon_px: i32) -> Self {
        let name = icons::display_name(path)
            .unwrap_or_else(|| Path::new(path).file_name().unwrap_or_default().to_string_lossy().into_owned());
        Item {
            path: path.to_string(),
            key: path.to_lowercase(),
            name_lower: name.to_lowercase(),
            name: name.encode_utf16().collect(),
            pixels: icons::image(path, icon_px),
            bmp: None,
            cell: (0, 0),
        }
    }

    fn matches(&self, query: &str) -> bool {
        self.name_lower.contains(query) || Path::new(&self.key).file_name().is_some_and(|n| n.to_string_lossy().contains(query))
    }
}

struct Fonts {
    dpi: u32,
    title: IDWriteTextFormat,
    label: IDWriteTextFormat,
}

/// Icon grid geometry in client pixels.
#[derive(Clone, Copy)]
struct Grid {
    cols: i32,
    rows: i32,
    x0: f32,
    y0: f32,
    cw: f32,
    ch: f32,
}

/// Edge positions a moving/resizing fence snaps to, per edge.
#[derive(Default)]
struct SnapTargets {
    l: Vec<i32>,
    t: Vec<i32>,
    r: Vec<i32>,
    b: Vec<i32>,
}

pub struct Fence {
    hwnd: HWND,
    pub cfg: FenceCfg,
    surface: Surface,
    fonts: Option<Fonts>,
    /// Items being shown: the active tab, or search results from every tab.
    items: Vec<Item>,
    /// Loaded items of the other tabs, so switching tabs is instant.
    stash: HashMap<String, Item>,
    /// Lowercased search query; empty when not searching.
    search: String,
    icon_px: i32,
    scroll: f32,
    hot: Option<usize>,
    /// Selected items by lowercased path, so selection survives reloads.
    selected: HashSet<String>,
    anchor: Option<usize>,
    drag: Option<Drag>,
    press: Option<Press>,
    band: Option<Band>,
    hover: bool,
    drop_hover: bool,
    marker: Option<Marker>,
    /// Auto-hidden down to the title bar.
    shrunk: bool,
    anim: Option<Anim>,
}

/// What the window procedure should do once the fence borrow is released.
enum Out {
    Default,
    Ret(isize),
    Save,
    Menu(POINT, Option<usize>),
    RenameTab(usize),
    RenameItem(usize),
    Verb(Vec<String>, &'static str),
    OpenSearch,
    ItemMenu(Vec<String>, POINT),
    Open(String),
    DragItems(Vec<String>),
    PrepareMenu(Vec<String>, bool),
}

pub unsafe fn register(hinst: HINSTANCE) -> Result<()> {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_DBLCLKS,
        lpfnWndProc: Some(wndproc),
        hInstance: hinst,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        lpszClassName: CLASS,
        ..Default::default()
    };
    if RegisterClassExW(&wc) == 0 {
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

pub unsafe fn create(hinst: HINSTANCE, mut cfg: FenceCfg) -> Result<HWND> {
    cfg.normalize();
    if cfg.catch_all {
        // The desktop is a single page.
        let items = cfg.tabs.drain(..).flat_map(|t| t.items).collect();
        cfg.tabs = vec![Tab { name: "桌面".into(), items }];
        cfg.active = 0;
        fit_work_area(&mut cfg);
    }
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        CLASS,
        w!("Lightfence"),
        WS_POPUP,
        cfg.x,
        cfg.y,
        cfg.w,
        cfg.h,
        // Owned by the desktop window: stays above the desktop, below normal
        // windows, and survives "show desktop".
        crate::desktop::host(),
        None,
        Some(hinst),
        None,
    )?;
    let surface = match Surface::new() {
        Ok(s) => s,
        Err(e) => {
            let _ = DestroyWindow(hwnd);
            return Err(e);
        }
    };
    let fence = Fence {
        hwnd,
        cfg,
        surface,
        fonts: None,
        items: Vec::new(),
        stash: HashMap::new(),
        search: String::new(),
        icon_px: 0,
        scroll: 0.0,
        hot: None,
        selected: HashSet::new(),
        anchor: None,
        drag: None,
        press: None,
        band: None,
        hover: false,
        drop_hover: false,
        marker: None,
        shrunk: false,
        anim: None,
    };
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(Box::new(RefCell::new(fence))) as isize);
    if let Err(e) = RegisterDragDrop(hwnd, &crate::drop::DropTarget::new(hwnd)) {
        crate::log(&format!("RegisterDragDrop failed: {e}"));
    }
    with(hwnd, |f| {
        f.refresh_items();
        f.redraw();
        f.arm_auto_hide();
    });
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    send_to_bottom(hwnd);
    Ok(hwnd)
}

/// The desktop layer covers the primary monitor's work area.
fn fit_work_area(cfg: &mut FenceCfg) {
    let mut wa = RECT::default();
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETWORKAREA, 0, Some(&mut wa as *mut RECT as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    cfg.x = wa.left;
    cfg.y = wa.top;
    cfg.w = wa.right - wa.left;
    cfg.h = wa.bottom - wa.top;
    cfg.collapsed = false;
}

/// For an owned window, HWND_BOTTOM lands it directly above its owner (the desktop).
unsafe fn send_to_bottom(hwnd: HWND) {
    let _ = SetWindowPos(hwnd, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
}

/// Runs `f` on the fence behind `hwnd`, if it exists and isn't already borrowed.
pub fn with<R>(hwnd: HWND, f: impl FnOnce(&mut Fence) -> R) -> Option<R> {
    let cell = unsafe { (GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<Fence>).as_ref()? };
    let mut fence = cell.try_borrow_mut().ok()?;
    Some(f(&mut fence))
}

pub unsafe fn set_owner(hwnd: HWND, owner: HWND) {
    SetWindowLongPtrW(hwnd, GWLP_HWNDPARENT, owner.0 as isize);
    send_to_bottom(hwnd);
}

/// Drag feedback: `Some(screen point)` while something droppable hovers the fence,
/// `None` when it leaves.
pub fn drag_over(hwnd: HWND, pt: Option<POINT>) {
    with(hwnd, |f| {
        let mut dirty = f.drop_hover != pt.is_some();
        f.drop_hover = pt.is_some();
        match pt {
            Some(_) => f.set_shrunk(false),
            None => f.arm_auto_hide(),
        }
        let marker = pt.and_then(|mut p| {
            unsafe {
                let _ = ScreenToClient(hwnd, &mut p);
            }
            if p.y < f.title_h() {
                // Hovering a tab opens it, so items can be dropped onto any tab.
                if let Zone::Tab(i) = f.hit(p.x, p.y) {
                    dirty |= f.switch_tab(i);
                }
                return None;
            }
            dirty |= f.autoscroll(p.y);
            f.search.is_empty().then(|| f.drop_marker(p))
        });
        dirty |= marker != f.marker;
        f.marker = marker;
        if dirty {
            f.redraw();
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut RefCell<Fence>;
            if !ptr.is_null() {
                drop(Box::from_raw(ptr));
            }
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        WM_DESTROY => {
            let _ = RevokeDragDrop(hwnd);
        }
        WM_WINDOWPOSCHANGING => {
            // Keep everything on the desktop layer: the desktop at the very bottom,
            // fences directly above it.
            let pos = &mut *(lp.0 as *mut WINDOWPOS);
            if !pos.flags.contains(SWP_NOZORDER) {
                match crate::desktop_layer().filter(|&l| l != hwnd) {
                    None => pos.hwndInsertAfter = HWND_BOTTOM,
                    Some(layer) => match GetWindow(layer, GW_HWNDPREV) {
                        Ok(above) if above == hwnd => pos.flags |= SWP_NOZORDER,
                        Ok(above) => pos.hwndInsertAfter = above,
                        Err(_) => pos.hwndInsertAfter = HWND_TOP,
                    },
                }
            }
            return LRESULT(0);
        }
        _ => {}
    }
    // Messages sent re-entrantly (e.g. by SetWindowPos during a drag) find the
    // fence borrowed and fall through to the default handling.
    match with(hwnd, |f| f.handle(msg, wp, lp)).unwrap_or(Out::Default) {
        Out::Default => DefWindowProcW(hwnd, msg, wp, lp),
        Out::Ret(r) => LRESULT(r),
        Out::Save => {
            crate::save();
            LRESULT(0)
        }
        Out::Menu(p, tab) => {
            fence_menu(hwnd, p, tab);
            LRESULT(0)
        }
        Out::RenameTab(i) => {
            open_rename(hwnd, i);
            LRESULT(0)
        }
        Out::OpenSearch => {
            open_search(hwnd);
            LRESULT(0)
        }
        Out::RenameItem(i) => {
            open_item_rename(hwnd, i);
            LRESULT(0)
        }
        Out::Verb(paths, verb) => {
            crate::shell::invoke_verb(hwnd, &paths, verb);
            LRESULT(0)
        }
        Out::ItemMenu(paths, p) => {
            item_menu(hwnd, paths, p);
            LRESULT(0)
        }
        Out::Open(path) => {
            open(&path);
            LRESULT(0)
        }
        Out::PrepareMenu(paths, in_fence) => {
            crate::shell::prepare(&paths, in_fence);
            LRESULT(0)
        }
        Out::DragItems(paths) => {
            crate::shell::drag_out(hwnd, &paths);
            // A move or delete by the drop target shows up via the desktop watcher.
            crate::sync_desktop();
            LRESULT(0)
        }
    }
}

/// Menu for the empty part of a fence (or the desktop); `tab` is the tab that was
/// right-clicked, if any.
unsafe fn fence_menu(hwnd: HWND, p: POINT, tab: Option<usize>) {
    if with(hwnd, |f| f.is_desktop()).unwrap_or(false) {
        desktop_menu(hwnd, p);
        return;
    }
    let Some((collapsed, tabs, active)) = with(hwnd, |f| (f.cfg.collapsed, f.cfg.tabs.len(), f.cfg.active)) else {
        return;
    };
    let tab = tab.unwrap_or(active);
    let mut items = vec![(1, "新建分区"), (7, "新建标签"), (8, "重命名")];
    if tabs > 1 {
        items.push((10, "删除此标签"));
    }
    items.extend([(0, ""), (2, if collapsed { "展开" } else { "折叠" }), (3, "删除此分区")]);
    items.extend([(0, ""), (11, "设置…"), (9, "退出 Lightfence")]);
    match crate::popup_menu(p, &items) {
        1 => crate::new_fence_at(p),
        2 => {
            with(hwnd, |f| f.toggle_collapsed());
            crate::save();
        }
        3 => crate::remove_fence(hwnd),
        7 => {
            if let Some(i) = with(hwnd, |f| f.add_tab()) {
                crate::save();
                open_rename(hwnd, i);
            }
        }
        8 => open_rename(hwnd, tab),
        10 => {
            with(hwnd, |f| f.delete_tab(tab));
            crate::save();
        }
        9 => crate::quit(),
        11 => crate::settings::show(),
        _ => {}
    }
}

// Our entries in the desktop background menu.
const BG_REFRESH: u32 = 1;
const BG_PASTE: u32 = 2;
const BG_PASTE_LINK: u32 = 3;
const BG_NEW_FENCE: u32 = 4;
const BG_SETTINGS: u32 = 5;
const BG_QUIT: u32 = 6;
const BG_SORT: u32 = 10; // + Sort as u32

#[derive(Clone, Copy)]
pub enum Sort {
    Name,
    Type,
    Size,
    Date,
}

/// Right-click on the empty desktop: Explorer's own background menu plus the
/// entries Explorer's desktop view normally adds (sort, refresh, paste).
unsafe fn desktop_menu(hwnd: HWND, p: POINT) {
    use crate::shell::BgChoice;
    let text = |s: &str| HSTRING::from(s);
    let choice = crate::shell::background_menu(
        crate::host(),
        p,
        |m| {
            if let Ok(sort) = CreatePopupMenu() {
                for (i, name) in ["名称", "项目类型", "大小", "修改日期"].iter().enumerate() {
                    let _ = AppendMenuW(sort, MF_STRING, (BG_SORT + i as u32) as usize, &text(name));
                }
                let _ = AppendMenuW(m, MF_POPUP, sort.0 as usize, &text("排序方式(&O)"));
            }
            let _ = AppendMenuW(m, MF_STRING, BG_REFRESH as usize, &text("刷新(&E)"));
            let _ = AppendMenuW(m, MF_SEPARATOR, 0, PCWSTR::null());
            let paste = if crate::shell::can_paste() { MF_STRING } else { MF_STRING | MF_GRAYED };
            let _ = AppendMenuW(m, paste, BG_PASTE as usize, &text("粘贴(&P)"));
            let _ = AppendMenuW(m, paste, BG_PASTE_LINK as usize, &text("粘贴快捷方式(&S)"));
        },
        |m| {
            let _ = AppendMenuW(m, MF_STRING, BG_NEW_FENCE as usize, &text("新建分区"));
            let _ = AppendMenuW(m, MF_STRING, BG_SETTINGS as usize, &text("Lightfence 设置…"));
            let _ = AppendMenuW(m, MF_STRING, BG_QUIT as usize, &text("退出 Lightfence"));
        },
    );
    match choice {
        BgChoice::Ours(BG_REFRESH) => crate::sync_desktop(),
        BgChoice::Ours(BG_PASTE) | BgChoice::Ours(BG_PASTE_LINK) => {
            crate::expect_new_items(p, false);
            crate::shell::paste_to_desktop(crate::host(), matches!(choice, BgChoice::Ours(BG_PASTE_LINK)));
        }
        BgChoice::Ours(BG_NEW_FENCE) => crate::new_fence_at(p),
        BgChoice::Ours(BG_SETTINGS) => crate::settings::show(),
        BgChoice::Ours(BG_QUIT) => crate::quit(),
        BgChoice::Ours(id) if (BG_SORT..BG_SORT + 4).contains(&id) => {
            let by = [Sort::Name, Sort::Type, Sort::Size, Sort::Date][(id - BG_SORT) as usize];
            with(hwnd, |f| f.arrange(by));
            crate::save();
        }
        // "新建" puts a new item on the desktop: drop it where the user clicked and rename it.
        BgChoice::Shell => crate::expect_new_items(p, true),
        _ => {}
    }
}

/// Starts renaming `path` if it is shown in this fence.
pub fn rename_path(hwnd: HWND, path: &str) {
    if let Some(i) = with(hwnd, |f| f.items.iter().position(|it| same_path(&it.path, path))).flatten() {
        open_item_rename(hwnd, i);
    }
}

/// Opens the rename box over tab `i`.
fn open_rename(hwnd: HWND, i: usize) {
    let Some((rect, name, dpi)) = with(hwnd, |f| {
        let r = *f.bar().tabs.get(i)?;
        let s = f.scale();
        let w = (r.right - r.left).max(160.0 * s);
        let (x, y) = (f.cfg.x as f32 + r.left, f.cfg.y as f32 + 3.0 * s);
        let rect = RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + f.title_h() as f32 - 6.0 * s) as i32 };
        Some((rect, f.cfg.tabs[i].name.clone(), f.dpi()))
    })
    .flatten() else {
        return;
    };
    editbox::open(hwnd, Purpose::RenameTab(i), rect, &name, None, dpi);
}

/// Opens the rename box over item `i`'s label, with the name (not the extension) selected.
fn open_item_rename(hwnd: HWND, i: usize) {
    let Some((rect, path, dpi)) = with(hwnd, |f| {
        let item = f.items.get(i)?;
        let g = f.grid();
        let cell = f.item_rect(&g, i);
        let s = f.scale();
        let w = (cell.right - cell.left).max(150.0 * s);
        let x = f.cfg.x as f32 + (cell.left + cell.right - w) / 2.0;
        let y = f.cfg.y as f32 + cell.top + 6.0 * s + f.icon_size() + 2.0 * s;
        let rect = RECT { left: x as i32, top: y as i32, right: (x + w) as i32, bottom: (y + 26.0 * s) as i32 };
        Some((rect, item.path.clone(), f.dpi()))
    })
    .flatten() else {
        return;
    };
    if crate::desktop::is_virtual(&path) {
        // 此电脑/回收站 are renamed through the shell's own UI elsewhere; keep it simple.
        return;
    }
    let name = crate::shell::editing_name(&path).unwrap_or_default();
    let is_dir = Path::new(&path).is_dir();
    let stem = match name.rfind('.') {
        Some(dot) if dot > 0 && !is_dir => Some(name[..dot].encode_utf16().count()),
        _ => None,
    };
    editbox::open(hwnd, Purpose::RenameItem(path), rect, &name, stem, dpi);
}

/// Opens the search box over the tab strip.
fn open_search(hwnd: HWND) {
    let Some((rect, query, dpi)) = with(hwnd, |f| {
        let s = f.scale();
        let b = f.bar();
        let (x, y) = (f.cfg.x as f32, f.cfg.y as f32);
        let left = x + f.title_h() as f32;
        let right = (x + b.search.left - 4.0 * s).max(left + 160.0 * s);
        let rect = RECT { left: left as i32, top: (y + 3.0 * s) as i32, right: right as i32, bottom: (y + f.title_h() as f32 - 3.0 * s) as i32 };
        (rect, f.search.clone(), f.dpi())
    }) else {
        return;
    };
    editbox::open(hwnd, Purpose::Search, rect, &query, None, dpi);
    with(hwnd, |f| f.redraw());
}

pub fn rename_tab(hwnd: HWND, i: usize, name: &str) {
    let name = name.trim();
    with(hwnd, |f| {
        if let (Some(tab), false) = (f.cfg.tabs.get_mut(i), name.is_empty()) {
            tab.name = name.to_string();
        }
        f.redraw();
    });
    crate::save();
}

pub fn editor_closed(hwnd: HWND) {
    with(hwnd, |f| f.redraw());
}

pub fn set_search(hwnd: HWND, query: &str) {
    with(hwnd, |f| f.set_search(query));
}

unsafe fn item_menu(hwnd: HWND, paths: Vec<String>, p: POINT) {
    use crate::shell::{ID_LOCATE, ID_OPEN, ID_REMOVE, ID_RENAME};
    let in_fence = with(hwnd, |f| !f.cfg.catch_all).unwrap_or(true);
    match crate::shell::context_menu(crate::host(), &paths, in_fence, p) {
        Some(ID_REMOVE) => {
            with(hwnd, |f| f.remove_items(&paths));
            crate::sync_desktop();
        }
        Some(ID_OPEN) => paths.iter().for_each(|p| open(p)),
        Some(ID_RENAME) => {
            if let Some(i) = with(hwnd, |f| f.items.iter().position(|it| it.path == paths[0])).flatten() {
                open_item_rename(hwnd, i);
            }
        }
        Some(ID_LOCATE) => {
            let args = HSTRING::from(format!("/select,\"{}\"", paths[0]));
            ShellExecuteW(None, w!("open"), w!("explorer.exe"), &args, PCWSTR::null(), SW_SHOWNORMAL);
        }
        // Shell commands (delete, rename…) reach us through the desktop watcher.
        _ => {}
    }
}

unsafe fn open(path: &str) {
    // Virtual items (此电脑, 回收站) open through their shell: URL.
    let target = if crate::desktop::is_virtual(path) { format!("shell:{path}") } else { path.to_string() };
    ShellExecuteW(None, PCWSTR::null(), &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
}

fn same_path(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32)
}

fn cursor_pos() -> POINT {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    p
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(vk.0 as i32) < 0 }
}

fn work_area(r: &RECT) -> Option<RECT> {
    unsafe {
        let mon = MonitorFromRect(r, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetMonitorInfoW(mon, &mut mi).as_bool().then_some(mi.rcWork)
    }
}

/// The smallest shift (within `range`) that puts `v` onto one of `targets`.
fn nearest(v: i32, targets: &[i32], range: i32) -> Option<i32> {
    targets.iter().map(|&t| t - v).filter(|d| d.abs() <= range).min_by_key(|d| d.abs())
}

fn intersects(a: &D2D_RECT_F, b: &D2D_RECT_F) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

impl Grid {
    fn clamp(&self, (c, r): (i32, i32)) -> (i32, i32) {
        (c.clamp(0, self.cols - 1), r.clamp(0, self.rows - 1))
    }

    /// Column-major slot number, the order Windows fills the desktop in.
    fn slot(&self, (c, r): (i32, i32)) -> i32 {
        c * self.rows + r
    }

    fn cell_of_slot(&self, n: i32) -> (i32, i32) {
        (n / self.rows, n % self.rows)
    }

    /// The free cell closest to `want`, searching outward ring by ring.
    fn nearest_free(&self, want: (i32, i32), taken: &HashSet<(i32, i32)>) -> (i32, i32) {
        let want = self.clamp(want);
        for d in 0..self.cols.max(self.rows) {
            for c in want.0 - d..=want.0 + d {
                for r in want.1 - d..=want.1 + d {
                    let on_ring = (c - want.0).abs() == d || (r - want.1).abs() == d;
                    if on_ring && (0..self.cols).contains(&c) && (0..self.rows).contains(&r) && !taken.contains(&(c, r)) {
                        return (c, r);
                    }
                }
            }
        }
        want
    }
}

impl Fence {
    fn is_desktop(&self) -> bool {
        self.cfg.catch_all
    }

    fn dpi(&self) -> u32 {
        unsafe { GetDpiForWindow(self.hwnd) }.max(96)
    }

    fn scale(&self) -> f32 {
        self.dpi() as f32 / 96.0
    }

    fn px(&self, v: f32) -> i32 {
        (v * self.scale()).round() as i32
    }

    /// Icon edge in pixels.
    fn icon_size(&self) -> f32 {
        crate::settings().icon_size as f32 * self.scale()
    }

    fn title_h(&self) -> i32 {
        if self.is_desktop() {
            0
        } else {
            self.px(34.0)
        }
    }

    fn visible_h(&self) -> i32 {
        if self.cfg.collapsed {
            return self.title_h();
        }
        if let Some(a) = self.anim {
            let t = (a.start.elapsed().as_secs_f32() * 1000.0 / ANIM_MS).min(1.0);
            let ease = 1.0 - (1.0 - t).powi(3);
            return (a.from + (a.to - a.from) * ease).round() as i32;
        }
        if self.shrunk {
            self.title_h()
        } else {
            self.cfg.h
        }
    }

    /// Top-left of the window: a fence on the bottom edge shrinks downwards,
    /// keeping its title bar against the edge.
    fn window_pos(&self) -> POINT {
        let mut y = self.cfg.y;
        if !self.cfg.collapsed && self.dock() == Some(Dock::Bottom) {
            y += self.cfg.h - self.visible_h();
        }
        POINT { x: self.cfg.x, y }
    }

    fn dock(&self) -> Option<Dock> {
        if self.is_desktop() {
            return None;
        }
        let r = RECT { left: self.cfg.x, top: self.cfg.y, right: self.cfg.x + self.cfg.w, bottom: self.cfg.y + self.cfg.h };
        let wa = work_area(&r)?;
        let tol = self.px(3.0);
        if (r.top - wa.top).abs() <= tol {
            Some(Dock::Top)
        } else if (r.bottom - wa.bottom).abs() <= tol {
            Some(Dock::Bottom)
        } else if (r.left - wa.left).abs() <= tol || (r.right - wa.right).abs() <= tol {
            Some(Dock::Side)
        } else {
            None
        }
    }

    fn can_auto_hide(&self) -> bool {
        crate::settings().auto_hide && !self.cfg.collapsed && self.dock().is_some()
    }

    /// Starts the countdown to shrinking, if this fence auto-hides.
    pub fn arm_auto_hide(&self) {
        if self.can_auto_hide() && !self.shrunk {
            unsafe { SetTimer(Some(self.hwnd), HIDE_TIMER, crate::settings().auto_hide_delay_ms.max(1), None) };
        }
    }

    /// Animates to shrunk (title bar only) or back to full height.
    fn set_shrunk(&mut self, shrink: bool) {
        if self.shrunk == shrink {
            return;
        }
        if !shrink {
            unsafe {
                let _ = KillTimer(Some(self.hwnd), HIDE_TIMER);
            }
        }
        let from = self.visible_h() as f32;
        self.shrunk = shrink;
        let to = if shrink { self.title_h() } else { self.cfg.h } as f32;
        self.anim = Some(Anim { from, to, start: std::time::Instant::now() });
        unsafe { SetTimer(Some(self.hwnd), ANIM_TIMER, 15, None) };
        self.redraw();
    }

    /// Whether something is going on that must keep the fence open.
    fn busy(&self) -> bool {
        let mut gui = GUITHREADINFO { cbSize: size_of::<GUITHREADINFO>() as u32, ..Default::default() };
        let in_menu = unsafe { GetGUIThreadInfo(0, &mut gui) }.is_ok() && (gui.flags.0 & (GUI_INMENUMODE.0 | GUI_POPUPMENUMODE.0)) != 0;
        let mut r = RECT::default();
        let under_mouse = unsafe { GetWindowRect(self.hwnd, &mut r) }.is_ok() && {
            let c = cursor_pos();
            c.x >= r.left && c.x < r.right && c.y >= r.top && c.y < r.bottom
        };
        in_menu || under_mouse || self.drag.is_some() || self.band.is_some() || self.drop_hover || editbox::purpose_for(self.hwnd).is_some()
    }

    /// Applies new settings: redraws, reloads icons at a new size, and starts or
    /// stops auto-hiding.
    pub fn settings_changed(&mut self, old: &Settings, new: &Settings) {
        if old.icon_size != new.icon_size {
            self.refresh_items();
        }
        if !self.can_auto_hide() {
            self.set_shrunk(false);
        } else if !old.auto_hide {
            self.arm_auto_hide();
        }
        self.redraw();
    }

    fn grid(&self) -> Grid {
        let s = self.scale();
        let icon = crate::settings().icon_size as f32;
        let (cw, ch) = ((icon + 36.0) * s, (icon + 44.0) * s);
        let (w, h) = (self.cfg.w as f32, self.cfg.h as f32);
        if self.is_desktop() {
            let m = 6.0 * s;
            let cols = (((w - 2.0 * m) / cw).floor() as i32).max(1);
            let rows = (((h - 2.0 * m) / ch).floor() as i32).max(1);
            return Grid { cols, rows, x0: m, y0: m, cw, ch };
        }
        let cols = (((w - 16.0 * s) / cw).floor() as i32).max(1);
        let rows = self.items.len().div_ceil(cols as usize) as i32;
        Grid { cols, rows, x0: ((w - cols as f32 * cw) / 2.0).max(0.0), y0: self.title_h() as f32 + 6.0 * s, cw, ch }
    }

    /// Where item `i` is drawn, in client pixels.
    fn item_rect(&self, g: &Grid, i: usize) -> D2D_RECT_F {
        let (c, r) = if self.is_desktop() { self.items[i].cell } else { (i as i32 % g.cols, i as i32 / g.cols) };
        let x = g.x0 + c as f32 * g.cw;
        let y = g.y0 + r as f32 * g.ch - self.scroll;
        D2D_RECT_F { left: x, top: y, right: x + g.cw, bottom: y + g.ch }
    }

    fn cell_at(&self, g: &Grid, x: i32, y: i32) -> (i32, i32) {
        let c = ((x as f32 - g.x0) / g.cw).floor() as i32;
        let r = ((y as f32 - g.y0 + self.scroll) / g.ch).floor() as i32;
        (c, r)
    }

    fn max_scroll(&self) -> f32 {
        if self.is_desktop() {
            return 0.0;
        }
        let g = self.grid();
        let content = g.y0 + g.rows as f32 * g.ch + 6.0 * self.scale();
        (content - self.visible_h() as f32).max(0.0)
    }

    fn item_at(&self, x: i32, y: i32) -> Option<usize> {
        if self.cfg.collapsed || y < self.title_h() {
            return None;
        }
        let g = self.grid();
        let (c, r) = self.cell_at(&g, x, y);
        if c < 0 || r < 0 || c >= g.cols {
            return None;
        }
        if self.is_desktop() {
            self.items.iter().position(|it| it.cell == (c, r))
        } else {
            let i = (r * g.cols + c) as usize;
            (i < self.items.len()).then_some(i)
        }
    }

    fn hit(&self, x: i32, y: i32) -> Zone {
        if self.is_desktop() {
            return Zone::Body;
        }
        let (w, h, e) = (self.cfg.w, self.visible_h(), self.px(6.0));
        let mut f = 0;
        if x < e {
            f |= L;
        } else if x >= w - e {
            f |= R;
        }
        if !self.cfg.collapsed {
            if y < self.px(4.0) {
                f |= T;
            } else if y >= h - e {
                f |= B;
            }
        }
        if f != 0 {
            return Zone::Resize(f);
        }
        if y >= self.title_h() {
            return Zone::Body;
        }
        if x < self.title_h() {
            return Zone::Chevron;
        }
        let bar = self.bar();
        if contains(&bar.search, x, y) {
            return if self.search.is_empty() { Zone::Search } else { Zone::ClearSearch };
        }
        if contains(&bar.plus, x, y) {
            return Zone::AddTab;
        }
        match bar.tabs.iter().position(|r| contains(r, x, y)) {
            Some(i) => Zone::Tab(i),
            None => Zone::Title,
        }
    }

    fn bar(&self) -> Bar {
        let s = self.scale();
        let (th, w) = (self.title_h() as f32, self.cfg.w as f32);
        let search = D2D_RECT_F { left: w - th - 2.0 * s, top: 0.0, right: w - 2.0 * s, bottom: th };
        let mut x = th - 4.0 * s;
        let mut tabs = Vec::new();
        if let (Some(fonts), true) = (&self.fonts, self.search.is_empty()) {
            for t in &self.cfg.tabs {
                let name: Vec<u16> = t.name.encode_utf16().collect();
                let tw = gfx::with(|g| unsafe { g.text_width(&name, &fonts.title) }) + 20.0 * s;
                tabs.push(D2D_RECT_F { left: x, top: 0.0, right: x + tw, bottom: th });
                x += tw;
            }
        }
        let plus = if self.search.is_empty() {
            let size = th * 0.8;
            D2D_RECT_F { left: x, top: (th - size) / 2.0, right: x + size, bottom: (th + size) / 2.0 }
        } else {
            D2D_RECT_F::default()
        };
        Bar { tabs, plus, search }
    }

    /// Shows tab `i`; returns whether anything changed.
    fn switch_tab(&mut self, i: usize) -> bool {
        if i >= self.cfg.tabs.len() || i == self.cfg.active || !self.search.is_empty() {
            return false;
        }
        self.cfg.active = i;
        self.scroll = 0.0;
        self.selected.clear();
        self.refresh_items();
        self.redraw();
        true
    }

    /// Adds an empty tab after the others, shows it, and returns its index.
    fn add_tab(&mut self) -> usize {
        self.cfg.tabs.push(Tab { name: "新标签".into(), items: Vec::new() });
        let i = self.cfg.tabs.len() - 1;
        self.switch_tab(i);
        i
    }

    /// Removes tab `i`; its items move to the neighbouring tab.
    fn delete_tab(&mut self, i: usize) {
        if self.cfg.tabs.len() < 2 || i >= self.cfg.tabs.len() {
            return;
        }
        let items = self.cfg.tabs.remove(i).items;
        let to = i.saturating_sub(1);
        self.cfg.tabs[to].items.extend(items);
        self.cfg.active = to;
        self.scroll = 0.0;
        self.refresh_items();
        self.redraw();
    }

    fn set_search(&mut self, query: &str) {
        let query = query.trim().to_lowercase();
        if query == self.search {
            return;
        }
        self.search = query;
        self.scroll = 0.0;
        self.selected.clear();
        self.refresh_items();
        self.redraw();
    }

    /// Rebuilds the item list from `cfg.items`, dropping files that no longer exist
    /// and reusing already-loaded icons. On the desktop, gives every item a cell.
    fn refresh_items(&mut self) {
        for tab in &mut self.cfg.tabs {
            tab.items.retain(|p| crate::desktop::is_virtual(p) || Path::new(p).exists());
        }
        let icon_px = self.icon_size().round() as i32;
        let mut pool: HashMap<String, Item> = self.items.drain(..).map(|i| (i.key.clone(), i)).collect();
        pool.extend(self.stash.drain());
        if icon_px != self.icon_px {
            pool.clear();
        }
        // Every tab's items stay loaded, so tab switches and searches are instant.
        let mut all: HashMap<String, Item> = HashMap::new();
        for p in self.cfg.all_items() {
            let k = p.to_lowercase();
            if !all.contains_key(&k) {
                let item = pool.remove(&k).unwrap_or_else(|| Item::load(p, icon_px));
                all.insert(k, item);
            }
        }
        let view: Vec<String> = if self.search.is_empty() {
            self.cfg.tab().items.clone()
        } else {
            self.cfg.all_items().filter(|p| all.get(&p.to_lowercase()).is_some_and(|it| it.matches(&self.search))).cloned().collect()
        };
        self.items = view.iter().filter_map(|p| all.remove(&p.to_lowercase())).collect();
        self.stash = all;
        self.icon_px = icon_px;
        if self.is_desktop() {
            self.place_cells();
        }
        let keys: HashSet<&str> = self.items.iter().map(|i| i.key.as_str()).collect();
        self.selected.retain(|k| keys.contains(k.as_str()));
        self.hot = None;
        self.anchor = None;
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// Keeps saved cells that are still valid and free; puts the rest in the
    /// first free cells, filling columns top to bottom like Windows does.
    fn place_cells(&mut self) {
        let g = self.grid();
        let keys: HashSet<String> = self.items.iter().map(|i| i.key.clone()).collect();
        self.cfg.cells.retain(|k, _| keys.contains(k));
        let mut taken = HashSet::new();
        let mut homeless = Vec::new();
        for (i, item) in self.items.iter_mut().enumerate() {
            match self.cfg.cells.get(&item.key).map(|c| (c[0], c[1])) {
                Some(c) if g.clamp(c) == c && taken.insert(c) => item.cell = c,
                _ => homeless.push(i),
            }
        }
        let mut next = 0;
        for i in homeless {
            while next < g.cols * g.rows && taken.contains(&g.cell_of_slot(next)) {
                next += 1;
            }
            // A full grid stacks the overflow in the last cell rather than losing it.
            let c = if next < g.cols * g.rows { g.cell_of_slot(next) } else { (g.cols - 1, g.rows - 1) };
            taken.insert(c);
            self.items[i].cell = c;
            self.cfg.cells.insert(self.items[i].key.clone(), [c.0, c.1]);
        }
    }

    /// Lines the desktop's items up column by column in the given order, like
    /// Explorer's "排序方式": 此电脑/回收站 first, then folders, then files.
    pub fn arrange(&mut self, by: Sort) {
        if !self.is_desktop() {
            return;
        }
        let meta: Vec<Option<std::fs::Metadata>> = self.items.iter().map(|i| std::fs::metadata(&i.path).ok()).collect();
        let group = |i: usize| {
            if crate::desktop::is_virtual(&self.items[i].path) {
                0
            } else if meta[i].as_ref().is_some_and(|m| m.is_dir()) {
                1
            } else {
                2
            }
        };
        let ext = |i: usize| Path::new(&self.items[i].key).extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        let size = |i: usize| meta[i].as_ref().map(|m| m.len()).unwrap_or(0);
        let date = |i: usize| meta[i].as_ref().and_then(|m| m.modified().ok());
        let mut order: Vec<usize> = (0..self.items.len()).collect();
        order.sort_by(|&a, &b| {
            let primary = match by {
                Sort::Name => std::cmp::Ordering::Equal,
                Sort::Type => ext(a).cmp(&ext(b)),
                Sort::Size => size(b).cmp(&size(a)),
                Sort::Date => date(b).cmp(&date(a)),
            };
            group(a).cmp(&group(b)).then(primary).then_with(|| self.items[a].name_lower.cmp(&self.items[b].name_lower))
        });
        let g = self.grid();
        self.cfg.cells.clear();
        for (slot, i) in order.into_iter().enumerate() {
            let c = g.cell_of_slot((slot as i32).min(g.cols * g.rows - 1));
            self.cfg.cells.insert(self.items[i].key.clone(), [c.0, c.1]);
        }
        self.refresh_items();
        self.redraw();
    }

    /// Re-fits the desktop to the work area after a display change.
    pub fn refit(&mut self) {
        if !self.is_desktop() {
            return;
        }
        fit_work_area(&mut self.cfg);
        self.refresh_items();
        self.redraw();
    }

    /// Drops items `keep` rejects (gone from the desktop, or claimed by another fence).
    pub fn retain_items(&mut self, keep: impl Fn(&str) -> bool) {
        let mut changed = false;
        for tab in &mut self.cfg.tabs {
            let before = tab.items.len();
            tab.items.retain(|p| keep(p));
            changed |= tab.items.len() != before;
        }
        if changed {
            self.refresh_items();
            self.redraw();
        }
    }

    /// Appends items not in this fence yet to the active tab.
    pub fn append_items(&mut self, paths: &[String]) {
        let new: Vec<String> = paths.iter().filter(|p| !self.cfg.all_items().any(|q| same_path(p, q))).cloned().collect();
        if !new.is_empty() {
            self.cfg.tab_mut().items.extend(new);
            self.refresh_items();
            self.redraw();
        }
    }

    /// Follows a rename on disk; returns whether this fence had the item.
    pub fn rename_item(&mut self, old: &str, new: &str) -> bool {
        let Some(slot) = self.cfg.tabs.iter_mut().flat_map(|t| t.items.iter_mut()).find(|q| same_path(q, old)) else { return false };
        *slot = new.to_string();
        if let Some(c) = self.cfg.cells.remove(&old.to_lowercase()) {
            self.cfg.cells.insert(new.to_lowercase(), c);
        }
        self.refresh_items();
        self.redraw();
        true
    }

    pub fn remove_items(&mut self, paths: &[String]) {
        self.retain_items(|q| !paths.iter().any(|p| same_path(p, q)));
    }

    /// Takes `paths` dropped at screen point `pt`: inserted at the marker in a
    /// fence, or placed on the grid around the drop cell on the desktop.
    pub fn accept_drop(&mut self, paths: &[String], pt: POINT) {
        let mut p = pt;
        unsafe {
            let _ = ScreenToClient(self.hwnd, &mut p);
        }
        if !self.is_desktop() {
            // Onto the tab strip or into search results: append to the active tab.
            let at = if p.y < self.title_h() || !self.search.is_empty() { usize::MAX } else { self.insert_pos(p.x, p.y).0 };
            self.insert_items(paths, at);
            return;
        }
        let g = self.grid();
        let target = g.clamp(self.cell_at(&g, p.x, p.y));
        let keys: Vec<String> = paths.iter().map(|p| p.to_lowercase()).collect();
        let current: Vec<Option<(i32, i32)>> =
            keys.iter().map(|k| self.items.iter().find(|i| i.key == *k).map(|i| i.cell)).collect();
        // Moving within the desktop keeps the group's shape; anything else lines
        // up down the column from the drop cell.
        let wanted: Vec<(i32, i32)> = match current.iter().copied().collect::<Option<Vec<_>>>() {
            Some(cells) => {
                let (dx, dy) = (target.0 - cells[0].0, target.1 - cells[0].1);
                cells.iter().map(|c| (c.0 + dx, c.1 + dy)).collect()
            }
            None => (0..keys.len() as i32).map(|n| g.cell_of_slot(g.slot(target) + n)).collect(),
        };
        let moving: HashSet<&String> = keys.iter().collect();
        let mut taken: HashSet<(i32, i32)> = self.items.iter().filter(|i| !moving.contains(&i.key)).map(|i| i.cell).collect();
        for (k, want) in keys.iter().zip(wanted) {
            let c = g.nearest_free(want, &taken);
            taken.insert(c);
            self.cfg.cells.insert(k.clone(), [c.0, c.1]);
        }
        for p in paths {
            if !self.cfg.all_items().any(|q| same_path(p, q)) {
                self.cfg.tab_mut().items.push(p.clone());
            }
        }
        self.selected = keys.into_iter().collect();
        self.refresh_items();
        self.redraw();
    }

    /// Inserts `paths` before index `at` (clamped), moving any that are already here.
    /// Moves `paths` into the active tab before index `at` (clamped), taking them
    /// off any other tab.
    fn insert_items(&mut self, paths: &[String], at: usize) {
        let active = self.cfg.active;
        for (i, tab) in self.cfg.tabs.iter_mut().enumerate() {
            if i != active {
                tab.items.retain(|q| !paths.iter().any(|p| same_path(p, q)));
            }
        }
        let items = &mut self.cfg.tabs[active].items;
        let mut at = at.min(items.len());
        let mut kept = Vec::with_capacity(items.len() + paths.len());
        for (i, q) in items.drain(..).enumerate() {
            if paths.iter().any(|p| same_path(p, &q)) {
                if i < at {
                    at -= 1;
                }
            } else {
                kept.push(q);
            }
        }
        kept.splice(at..at, paths.iter().cloned());
        *items = kept;
        self.selected = paths.iter().map(|p| p.to_lowercase()).collect();
        self.refresh_items();
        self.redraw();
    }

    /// Insertion slot for a drop at client point (x, y).
    fn insert_pos(&self, x: i32, y: i32) -> (usize, bool) {
        let n = self.items.len();
        if n == 0 {
            return (0, false);
        }
        let g = self.grid();
        let row = ((y as f32 - g.y0 + self.scroll) / g.ch).floor().max(0.0) as usize;
        let col = ((x as f32 - g.x0) / g.cw).round().clamp(0.0, g.cols as f32) as usize;
        let i = (row * g.cols as usize + col).min(n);
        (i, i > 0 && (col == g.cols as usize || i == n))
    }

    fn drop_marker(&self, p: POINT) -> Marker {
        if self.is_desktop() {
            let g = self.grid();
            let (c, r) = g.clamp(self.cell_at(&g, p.x, p.y));
            Marker::Cell(c, r)
        } else {
            let (i, after) = self.insert_pos(p.x, p.y);
            Marker::Bar(i, after)
        }
    }

    /// Scrolls when a drag hovers near the top or bottom edge; returns whether it moved.
    fn autoscroll(&mut self, y: i32) -> bool {
        if self.is_desktop() {
            return false;
        }
        let edge = self.px(24.0);
        let step = self.grid().ch * 0.2;
        let scroll = if y < self.title_h() + edge {
            self.scroll - step
        } else if y > self.visible_h() - edge {
            self.scroll + step
        } else {
            return false;
        };
        let scroll = scroll.clamp(0.0, self.max_scroll());
        let moved = scroll != self.scroll;
        self.scroll = scroll;
        moved
    }

    /// Selected paths in display order, with `first` (the grabbed item) leading.
    fn selected_paths(&self, first: usize) -> Vec<String> {
        let mut paths = vec![self.items[first].path.clone()];
        paths.extend(
            self.items.iter().enumerate().filter(|(i, it)| *i != first && self.selected.contains(&it.key)).map(|(_, it)| it.path.clone()),
        );
        paths
    }

    fn take_focus(&self) {
        unsafe {
            if GetForegroundWindow() != self.hwnd {
                crate::take_foreground(self.hwnd);
            }
            let _ = SetFocus(Some(self.hwnd));
        }
    }

    /// Explorer's keys for the selection.
    fn key(&mut self, vk: u16) -> Out {
        let sel: Vec<usize> = (0..self.items.len()).filter(|&i| self.selected.contains(&self.items[i].key)).collect();
        let first = self.anchor.filter(|a| sel.contains(a)).or(sel.first().copied());
        match VIRTUAL_KEY(vk) {
            VK_F2 => match first {
                Some(i) => Out::RenameItem(i),
                None => Out::Ret(0),
            },
            VK_DELETE => match first {
                Some(i) => Out::Verb(self.selected_paths(i), "delete"),
                None => Out::Ret(0),
            },
            VK_RETURN => match first {
                Some(i) => Out::Open(self.items[i].path.clone()),
                None => Out::Ret(0),
            },
            VK_A if key_down(VK_CONTROL) => {
                self.selected = self.items.iter().map(|i| i.key.clone()).collect();
                self.redraw();
                Out::Ret(0)
            }
            VK_ESCAPE => {
                self.selected.clear();
                self.redraw();
                Out::Ret(0)
            }
            _ => Out::Default,
        }
    }

    fn select_only(&mut self, i: usize) {
        self.selected.clear();
        self.selected.insert(self.items[i].key.clone());
        self.anchor = Some(i);
    }

    /// Click on item `i` with the current modifier keys.
    fn click_select(&mut self, i: usize) -> bool {
        let key = self.items[i].key.clone();
        if key_down(VK_SHIFT) {
            let a = self.anchor.unwrap_or(i);
            if !key_down(VK_CONTROL) {
                self.selected.clear();
            }
            for it in &self.items[a.min(i)..=a.max(i)] {
                self.selected.insert(it.key.clone());
            }
            false
        } else if key_down(VK_CONTROL) {
            if !self.selected.remove(&key) {
                self.selected.insert(key);
            }
            self.anchor = Some(i);
            false
        } else if self.selected.contains(&key) {
            self.anchor = Some(i);
            true
        } else {
            self.select_only(i);
            false
        }
    }

    fn update_band(&mut self, cur: POINT) {
        let Some(band) = self.band.as_mut() else { return };
        band.cur = cur;
        let r = D2D_RECT_F {
            left: band.start.x.min(cur.x) as f32,
            top: band.start.y.min(cur.y) as f32,
            right: band.start.x.max(cur.x) as f32,
            bottom: band.start.y.max(cur.y) as f32,
        };
        let mut sel = band.base.clone();
        let g = self.grid();
        let s = self.scale();
        for i in 0..self.items.len() {
            let c = self.item_rect(&g, i);
            // Hit the visible part of the cell (icon and label), not its padding.
            let body = D2D_RECT_F { left: c.left + 12.0 * s, top: c.top + 4.0 * s, right: c.right - 12.0 * s, bottom: c.bottom - 4.0 * s };
            if intersects(&r, &body) {
                sel.insert(self.items[i].key.clone());
            }
        }
        self.selected = sel;
    }

    fn toggle_collapsed(&mut self) {
        self.cfg.collapsed = !self.cfg.collapsed;
        self.redraw();
    }

    fn redraw(&mut self) {
        if let Err(e) = self.render() {
            crate::log(&format!("render failed: {e}"));
        }
    }

    fn render(&mut self) -> Result<()> {
        match self.render_once() {
            Err(e) if e.code() == D2DERR_RECREATE_TARGET => {
                // Device lost: bitmaps belong to the old target, rebuild them from the cached pixels.
                self.surface = unsafe { Surface::new() }?;
                self.items.iter_mut().for_each(|i| i.bmp = None);
                self.render_once()
            }
            r => r,
        }
    }

    fn render_once(&mut self) -> Result<()> {
        let dpi = self.dpi();
        let s = self.scale();
        if self.fonts.as_ref().map(|f| f.dpi) != Some(dpi) {
            let (title, label) = gfx::with(|g| unsafe { Ok::<_, windows::core::Error>((g.text_format(13.5 * s, true)?, g.label_format(12.0 * s)?)) })?;
            self.fonts = Some(Fonts { dpi, title, label });
        }
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        for item in &mut self.items {
            if let (None, Some(px)) = (&item.bmp, &item.pixels) {
                let size = D2D_SIZE_U { width: px.w, height: px.h };
                item.bmp = unsafe { self.surface.rt.CreateBitmap(size, Some(px.data.as_ptr() as _), px.w * 4, &props) }.ok();
            }
        }

        let fonts = self.fonts.as_ref().unwrap();
        let (title_fmt, label_fmt) = (fonts.title.clone(), fonts.label.clone());
        let bar = self.bar();
        let tab_names: Vec<Vec<u16>> = self.cfg.tabs.iter().map(|t| t.name.encode_utf16().collect()).collect();
        let (active, multi_tab) = (self.cfg.active, self.cfg.tabs.len() > 1);
        let searching = !self.search.is_empty();
        let search_label: Vec<u16> = format!("搜索结果（{}）", self.items.len()).encode_utf16().collect();
        let search_box_open = editbox::purpose_for(self.hwnd) == Some(Purpose::Search);
        let (w, h) = (self.cfg.w, self.visible_h());
        let th = self.title_h() as f32;
        let (hover, drop_hover, collapsed, desktop) = (self.hover, self.drop_hover, self.cfg.collapsed, self.is_desktop());
        let pos = self.window_pos();
        let look = crate::settings();
        let g = self.grid();
        let cells: Vec<D2D_RECT_F> = (0..self.items.len()).map(|i| self.item_rect(&g, i)).collect();
        let selected: Vec<bool> = self.items.iter().map(|i| self.selected.contains(&i.key)).collect();
        let (hot, marker) = (self.hot, self.marker);
        let band = self.band.as_ref().map(|b| D2D_RECT_F {
            left: b.start.x.min(b.cur.x) as f32,
            top: b.start.y.min(b.cur.y) as f32,
            right: b.start.x.max(b.cur.x) as f32,
            bottom: b.start.y.max(b.cur.y) as f32,
        });
        let (scroll, max_scroll) = (self.scroll, self.max_scroll());
        let icon = self.icon_size();
        let items = &self.items;

        unsafe {
            self.surface.present(self.hwnd, pos, w, h, |rt| {
                let (wf, hf) = (w as f32, h as f32);
                let fg = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 0.92), None)?;
                if desktop {
                    // Nearly invisible, but not fully transparent, so the empty desktop
                    // still receives clicks (rubber band, right-click menu).
                    let catch = rt.CreateSolidColorBrush(&rgba(0.0, 0.0, 0.0, 1.0 / 255.0), None)?;
                    rt.FillRectangle(&D2D_RECT_F { left: 0.0, top: 0.0, right: wf, bottom: hf }, &catch);
                } else {
                    let radius = 8.0 * s;
                    let shape = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F { left: 0.5, top: 0.5, right: wf - 0.5, bottom: hf - 0.5 },
                        radiusX: radius,
                        radiusY: radius,
                    };
                    // ~30% black veil, measured from XiaoZhi Desktop's fences; only a drag hovering over it changes the tone.
                    let [r, g, b] = look.color.map(|c| c as f32 / 255.0);
                    let alpha = look.opacity as f32 / 100.0;
                    let bg = rt.CreateSolidColorBrush(&rgba(r, g, b, if drop_hover { (alpha + 0.12).min(1.0) } else { alpha }), None)?;
                    rt.FillRoundedRectangle(&shape, &bg);
                    let border = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, if drop_hover { 0.6 } else { 0.10 }), None)?;
                    rt.DrawRoundedRectangle(&shape, &border, if drop_hover { 2.0 * s } else { 1.0 }, None);

                    // Chevron: "^" when expanded, ">" when collapsed.
                    let (cx, cy, d) = (th * 0.5, th * 0.5, 4.0 * s);
                    let (a, b, c) = if collapsed {
                        ((cx - d * 0.5, cy - d), (cx + d * 0.5, cy), (cx - d * 0.5, cy + d))
                    } else {
                        ((cx - d, cy + d * 0.5), (cx, cy - d * 0.5), (cx + d, cy + d * 0.5))
                    };
                    let v = |(x, y): (f32, f32)| Vector2 { X: x, Y: y };
                    rt.DrawLine(v(a), v(b), &fg, 1.6 * s, None);
                    rt.DrawLine(v(b), v(c), &fg, 1.6 * s, None);
                    let dim = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 0.55), None)?;
                    let accent = rt.CreateSolidColorBrush(&rgba(0.2, 0.72, 1.0, 1.0), None)?;
                    rt.PushAxisAlignedClip(&D2D_RECT_F { left: th - 4.0 * s, top: 0.0, right: bar.search.left, bottom: th }, D2D1_ANTIALIAS_MODE_ALIASED);
                    if searching && !search_box_open {
                        let text_box = D2D_RECT_F { left: th + 6.0 * s, top: 0.0, right: bar.search.left, bottom: th };
                        rt.DrawText(&search_label, &title_fmt, &text_box, &fg, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
                    }
                    for (i, r) in bar.tabs.iter().enumerate() {
                        let current = i == active;
                        let text_box = D2D_RECT_F { left: r.left + 10.0 * s, top: 0.0, right: r.right, bottom: th };
                        rt.DrawText(&tab_names[i], &title_fmt, &text_box, if current { &fg } else { &dim }, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
                        if current && multi_tab {
                            let line = D2D_RECT_F { left: r.left + 10.0 * s, top: th - 4.0 * s, right: r.right - 10.0 * s, bottom: th - 2.0 * s };
                            rt.FillRectangle(&line, &accent);
                        }
                    }
                    if !searching {
                        // "+": new tab.
                        let (px, py, d) = ((bar.plus.left + bar.plus.right) / 2.0, (bar.plus.top + bar.plus.bottom) / 2.0, 5.0 * s);
                        rt.DrawLine(v((px - d, py)), v((px + d, py)), &dim, 1.5 * s, None);
                        rt.DrawLine(v((px, py - d)), v((px, py + d)), &dim, 1.5 * s, None);
                    }
                    rt.PopAxisAlignedClip();
                    // Search button, or "×" to leave search results.
                    let (sx, sy) = ((bar.search.left + bar.search.right) / 2.0, th / 2.0);
                    if searching {
                        let d = 4.5 * s;
                        rt.DrawLine(v((sx - d, sy - d)), v((sx + d, sy + d)), &fg, 1.5 * s, None);
                        rt.DrawLine(v((sx - d, sy + d)), v((sx + d, sy - d)), &fg, 1.5 * s, None);
                    } else {
                        let r = 5.0 * s;
                        let lens = D2D1_ELLIPSE { point: Vector2 { X: sx - 1.5 * s, Y: sy - 1.5 * s }, radiusX: r, radiusY: r };
                        rt.DrawEllipse(&lens, &dim, 1.5 * s, None);
                        rt.DrawLine(v((sx + 2.0 * s, sy + 2.0 * s)), v((sx + 6.0 * s, sy + 6.0 * s)), &dim, 1.5 * s, None);
                    }
                    if collapsed {
                        return Ok(());
                    }
                    let sep = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 0.12), None)?;
                    rt.FillRectangle(&D2D_RECT_F { left: 10.0 * s, top: th - 1.0, right: wf - 10.0 * s, bottom: th }, &sep);
                }

                let body = D2D_RECT_F { left: 0.0, top: th, right: wf, bottom: hf - 1.0 };
                rt.PushAxisAlignedClip(&body, D2D1_ANTIALIAS_MODE_ALIASED);
                let hot_bg = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, 0.12), None)?;
                let sel_bg = rt.CreateSolidColorBrush(&rgba(0.55, 0.75, 1.0, 0.30), None)?;
                let sel_border = rt.CreateSolidColorBrush(&rgba(0.7, 0.85, 1.0, 0.55), None)?;
                let shadow = rt.CreateSolidColorBrush(&rgba(0.0, 0.0, 0.0, 0.75), None)?;
                for (i, (item, cell)) in items.iter().zip(&cells).enumerate() {
                    if cell.bottom < th || cell.top > hf {
                        continue;
                    }
                    let r = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F { left: cell.left + 2.0 * s, top: cell.top + 2.0 * s, right: cell.right - 2.0 * s, bottom: cell.bottom - 2.0 * s },
                        radiusX: 6.0 * s,
                        radiusY: 6.0 * s,
                    };
                    if selected[i] {
                        rt.FillRoundedRectangle(&r, &sel_bg);
                        rt.DrawRoundedRectangle(&r, &sel_border, 1.0, None);
                    } else if hot == Some(i) {
                        rt.FillRoundedRectangle(&r, &hot_bg);
                    }
                    let (ix, iy) = (cell.left + (g.cw - icon) / 2.0, cell.top + 6.0 * s);
                    if let (Some(bmp), Some(px)) = (&item.bmp, &item.pixels) {
                        // Shell images are fitted into the icon box; draw them centered at native size.
                        let k = (icon / px.w.max(px.h) as f32).min(1.0);
                        let (bw, bh) = (px.w as f32 * k, px.h as f32 * k);
                        let dst = D2D_RECT_F {
                            left: ix + (icon - bw) / 2.0,
                            top: iy + (icon - bh) / 2.0,
                            right: ix + (icon + bw) / 2.0,
                            bottom: iy + (icon + bh) / 2.0,
                        };
                        rt.DrawBitmap(bmp, Some(&dst), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
                    }
                    let label = D2D_RECT_F { left: cell.left + 3.0 * s, top: iy + icon + 4.0 * s, right: cell.right - 3.0 * s, bottom: cell.bottom - 2.0 * s };
                    let offset = D2D_RECT_F { left: label.left + s, top: label.top + s, right: label.right + s, bottom: label.bottom + s };
                    rt.DrawText(&item.name, &label_fmt, &offset, &shadow, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
                    rt.DrawText(&item.name, &label_fmt, &label, &fg, D2D1_DRAW_TEXT_OPTIONS_CLIP, DWRITE_MEASURING_MODE_NATURAL);
                }

                let accent = rt.CreateSolidColorBrush(&rgba(0.2, 0.72, 1.0, 1.0), None)?;
                match marker {
                    // Insertion bar before the slot, or after the previous cell at the end of a row.
                    Some(Marker::Bar(i, after)) if !cells.is_empty() => {
                        let c = if after { cells[i - 1] } else { cells[i] };
                        let x = if after { c.right } else { c.left };
                        let bar = D2D_RECT_F { left: x - 1.5 * s, top: c.top + 8.0 * s, right: x + 1.5 * s, bottom: c.bottom - 8.0 * s };
                        rt.FillRectangle(&bar, &accent);
                    }
                    Some(Marker::Cell(c, r)) => {
                        let (x, y) = (g.x0 + c as f32 * g.cw, g.y0 + r as f32 * g.ch);
                        let slot = D2D1_ROUNDED_RECT {
                            rect: D2D_RECT_F { left: x + 3.0 * s, top: y + 3.0 * s, right: x + g.cw - 3.0 * s, bottom: y + g.ch - 3.0 * s },
                            radiusX: 6.0 * s,
                            radiusY: 6.0 * s,
                        };
                        rt.DrawRoundedRectangle(&slot, &accent, 2.0 * s, None);
                    }
                    _ => {}
                }
                if let Some(b) = band {
                    let fill = rt.CreateSolidColorBrush(&rgba(0.25, 0.55, 1.0, 0.18), None)?;
                    let edge = rt.CreateSolidColorBrush(&rgba(0.45, 0.72, 1.0, 0.85), None)?;
                    rt.FillRectangle(&b, &fill);
                    rt.DrawRectangle(&b, &edge, 1.0, None);
                }
                rt.PopAxisAlignedClip();

                if max_scroll > 0.0 {
                    let track = hf - th - 8.0 * s;
                    let thumb = (track * track / (track + max_scroll)).max(24.0 * s);
                    let top = th + 4.0 * s + (track - thumb) * scroll / max_scroll;
                    let bar = D2D1_ROUNDED_RECT {
                        rect: D2D_RECT_F { left: wf - 7.0 * s, top, right: wf - 3.0 * s, bottom: top + thumb },
                        radiusX: 2.0 * s,
                        radiusY: 2.0 * s,
                    };
                    let bar_brush = rt.CreateSolidColorBrush(&rgba(1.0, 1.0, 1.0, if hover { 0.45 } else { 0.25 }), None)?;
                    rt.FillRoundedRectangle(&bar, &bar_brush);
                }
                Ok(())
            })
        }
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Out {
        match msg {
            WM_MOUSEACTIVATE => Out::Ret(MA_NOACTIVATE as isize),
            WM_SETCURSOR => {
                let zone = match self.drag {
                    Some(d) => d.zone,
                    None => {
                        let mut p = cursor_pos();
                        unsafe {
                            let _ = ScreenToClient(self.hwnd, &mut p);
                        }
                        self.hit(p.x, p.y)
                    }
                };
                unsafe {
                    SetCursor(LoadCursorW(None, cursor_for(zone)).ok());
                }
                Out::Ret(1)
            }
            WM_MOUSEMOVE => {
                let mut dirty = false;
                // Reaching a shrunk fence's title bar brings it back out.
                self.set_shrunk(false);
                if !self.hover {
                    self.hover = true;
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                    dirty = true;
                }
                let (x, y) = lparam_xy(lp);
                if let Some(press) = self.press {
                    let c = cursor_pos();
                    let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXDRAG), GetSystemMetrics(SM_CYDRAG)) };
                    if (c.x - press.at.x).abs() > cx || (c.y - press.at.y).abs() > cy {
                        self.press = None;
                        self.selected.insert(self.items[press.item].key.clone());
                        self.redraw();
                        return Out::DragItems(self.selected_paths(press.item));
                    }
                }
                if self.band.is_some() {
                    self.update_band(POINT { x, y });
                    dirty = true;
                } else if self.drag.is_some() {
                    self.drag_to(cursor_pos());
                } else {
                    let hot = self.item_at(x, y);
                    dirty |= hot != self.hot;
                    self.hot = hot;
                }
                if dirty {
                    self.redraw();
                }
                Out::Ret(0)
            }
            WM_MOUSELEAVE => {
                self.hover = false;
                self.hot = None;
                self.redraw();
                self.arm_auto_hide();
                Out::Ret(0)
            }
            WM_TIMER if wp.0 == HIDE_TIMER => {
                unsafe {
                    let _ = KillTimer(Some(self.hwnd), HIDE_TIMER);
                }
                if self.busy() {
                    self.arm_auto_hide();
                } else if self.can_auto_hide() {
                    self.set_shrunk(true);
                }
                Out::Ret(0)
            }
            WM_TIMER if wp.0 == ANIM_TIMER => {
                let done = self.anim.is_none_or(|a| a.start.elapsed().as_secs_f32() * 1000.0 >= ANIM_MS);
                if done {
                    self.anim = None;
                    unsafe {
                        let _ = KillTimer(Some(self.hwnd), ANIM_TIMER);
                    }
                }
                self.redraw();
                Out::Ret(0)
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as f32;
                let step = self.grid().ch;
                let scroll = (self.scroll - delta / 120.0 * step).clamp(0.0, self.max_scroll());
                if scroll != self.scroll {
                    self.scroll = scroll;
                    let mut p = cursor_pos();
                    unsafe {
                        let _ = ScreenToClient(self.hwnd, &mut p);
                    }
                    self.hot = self.item_at(p.x, p.y);
                    self.redraw();
                }
                Out::Ret(0)
            }
            WM_LBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                let zone = self.hit(x, y);
                match zone {
                    Zone::AddTab => return Out::RenameTab(self.add_tab()),
                    Zone::Search => return Out::OpenSearch,
                    Zone::ClearSearch => {
                        self.set_search("");
                        return Out::Ret(0);
                    }
                    Zone::Tab(i) => {
                        self.switch_tab(i);
                    }
                    _ => {}
                }
                if zone != Zone::Body {
                    let c = &self.cfg;
                    self.drag = Some(Drag { zone, cursor: cursor_pos(), x: c.x, y: c.y, w: c.w, h: c.h, moved: false });
                    unsafe { SetCapture(self.hwnd) };
                    return Out::Ret(0);
                }
                // Take keyboard focus like the desktop does, for F2 / Delete / Ctrl+A.
                self.take_focus();
                match self.item_at(x, y) {
                    Some(i) => {
                        let narrow = self.click_select(i);
                        self.press = Some(Press { item: i, at: cursor_pos(), narrow_on_release: narrow });
                    }
                    None => {
                        let base = if key_down(VK_CONTROL) { self.selected.clone() } else { HashSet::new() };
                        self.selected = base.clone();
                        self.band = Some(Band { start: POINT { x, y }, cur: POINT { x, y }, base });
                        unsafe { SetCapture(self.hwnd) };
                    }
                }
                self.redraw();
                Out::Ret(0)
            }
            WM_LBUTTONUP => {
                if let Some(p) = self.press.take() {
                    if p.narrow_on_release {
                        self.select_only(p.item);
                        self.redraw();
                    }
                }
                if self.band.take().is_some() {
                    unsafe {
                        let _ = ReleaseCapture();
                    }
                    self.redraw();
                }
                let Some(d) = self.drag.take() else { return Out::Ret(0) };
                unsafe {
                    let _ = ReleaseCapture();
                }
                match d.zone {
                    _ if d.moved => Out::Save,
                    Zone::Chevron => {
                        self.toggle_collapsed();
                        Out::Save
                    }
                    // A click on a tab switched to it.
                    Zone::Tab(_) => Out::Save,
                    _ => Out::Ret(0),
                }
            }
            WM_CAPTURECHANGED => {
                self.drag = None;
                if self.band.take().is_some() {
                    self.redraw();
                }
                Out::Ret(0)
            }
            WM_LBUTTONDBLCLK => {
                let (x, y) = lparam_xy(lp);
                match self.hit(x, y) {
                    Zone::Title => {
                        self.toggle_collapsed();
                        Out::Save
                    }
                    Zone::Tab(i) => Out::RenameTab(i),
                    Zone::Body => match self.item_at(x, y) {
                        Some(i) => Out::Open(self.items[i].path.clone()),
                        None => Out::Ret(0),
                    },
                    _ => Out::Ret(0),
                }
            }
            WM_KEYDOWN => self.key(wp.0 as u16),
            WM_RBUTTONDOWN => {
                let (x, y) = lparam_xy(lp);
                self.take_focus();
                let Some(i) = self.item_at(x, y) else { return Out::Ret(0) };
                if !self.selected.contains(&self.items[i].key) {
                    self.select_only(i);
                    self.redraw();
                }
                Out::PrepareMenu(self.selected_paths(i), !self.is_desktop())
            }
            WM_RBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                if y < self.title_h() {
                    let tab = match self.hit(x, y) {
                        Zone::Tab(i) => Some(i),
                        _ => None,
                    };
                    return Out::Menu(cursor_pos(), tab);
                }
                match self.item_at(x, y) {
                    Some(i) => Out::ItemMenu(self.selected_paths(i), cursor_pos()),
                    None => {
                        if !self.selected.is_empty() {
                            self.selected.clear();
                            self.redraw();
                        }
                        Out::Menu(cursor_pos(), None)
                    }
                }
            }
            WM_DPICHANGED => {
                self.refresh_items();
                self.redraw();
                Out::Ret(0)
            }
            _ => Out::Default,
        }
    }

    fn snap_targets(&self, me: &RECT) -> SnapTargets {
        let (range, gap) = (self.px(12.0), self.px(4.0));
        let mut st = SnapTargets::default();
        for o in crate::fence_rects(self.hwnd) {
            // Side by side: meet edge to edge, or line up the outer edges.
            if me.top < o.bottom + range && me.bottom > o.top - range {
                st.l.extend([o.right + gap, o.left]);
                st.r.extend([o.left - gap, o.right]);
            }
            // Stacked.
            if me.left < o.right + range && me.right > o.left - range {
                st.t.extend([o.bottom + gap, o.top]);
                st.b.extend([o.top - gap, o.bottom]);
            }
        }
        if let Some(wa) = work_area(me) {
            st.l.push(wa.left);
            st.r.push(wa.right);
            st.t.push(wa.top);
            st.b.push(wa.bottom);
        }
        st
    }

    fn drag_to(&mut self, c: POINT) {
        let Some(d) = self.drag.as_mut() else { return };
        let (dx, dy) = (c.x - d.cursor.x, c.y - d.cursor.y);
        if !d.moved && dx.abs() < 3 && dy.abs() < 3 {
            return;
        }
        d.moved = true;
        let d = *d;
        // Holding Alt turns snapping off.
        let snap = !key_down(VK_MENU);
        let range = self.px(12.0);
        match d.zone {
            // Dragging a tab moves the fence too, like its title bar.
            Zone::Title | Zone::Chevron | Zone::Tab(_) => {
                let (w, h) = (self.cfg.w, self.visible_h());
                let (mut x, mut y) = (d.x + dx, d.y + dy);
                if snap {
                    let me = RECT { left: x, top: y, right: x + w, bottom: y + h };
                    let st = self.snap_targets(&me);
                    let best = |a: Option<i32>, b: Option<i32>| [a, b].into_iter().flatten().min_by_key(|v| v.abs()).unwrap_or(0);
                    x += best(nearest(me.left, &st.l, range), nearest(me.right, &st.r, range));
                    y += best(nearest(me.top, &st.t, range), nearest(me.bottom, &st.b, range));
                }
                self.cfg.x = x;
                self.cfg.y = y;
                unsafe {
                    let _ = SetWindowPos(self.hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
                }
            }
            Zone::Resize(f) => {
                let (min_w, min_h) = (self.px(160.0), self.title_h() + self.px(48.0));
                let (mut l, mut t, mut r, mut b) = (d.x, d.y, d.x + d.w, d.y + d.h);
                if f & L != 0 {
                    l += dx;
                }
                if f & R != 0 {
                    r += dx;
                }
                if f & T != 0 {
                    t += dy;
                }
                if f & B != 0 {
                    b += dy;
                }
                if snap {
                    let st = self.snap_targets(&RECT { left: l, top: t, right: r, bottom: b });
                    if f & L != 0 {
                        l += nearest(l, &st.l, range).unwrap_or(0);
                    }
                    if f & R != 0 {
                        r += nearest(r, &st.r, range).unwrap_or(0);
                    }
                    if f & T != 0 {
                        t += nearest(t, &st.t, range).unwrap_or(0);
                    }
                    if f & B != 0 {
                        b += nearest(b, &st.b, range).unwrap_or(0);
                    }
                }
                // Enforce the minimum size on whichever edge is being dragged.
                if f & L != 0 {
                    l = l.min(r - min_w);
                } else {
                    r = r.max(l + min_w);
                }
                if f & T != 0 {
                    t = t.min(b - min_h);
                } else {
                    b = b.max(t + min_h);
                }
                self.cfg.x = l;
                self.cfg.y = t;
                self.cfg.w = r - l;
                self.cfg.h = b - t;
                self.scroll = self.scroll.min(self.max_scroll());
                self.redraw();
            }
            _ => {}
        }
    }
}

fn cursor_for(zone: Zone) -> PCWSTR {
    match zone {
        Zone::Resize(L | R) => IDC_SIZEWE,
        Zone::Resize(T | B) => IDC_SIZENS,
        Zone::Resize(f) if f == L | T || f == R | B => IDC_SIZENWSE,
        Zone::Resize(_) => IDC_SIZENESW,
        Zone::Chevron | Zone::AddTab | Zone::Search | Zone::ClearSearch => IDC_HAND,
        _ => IDC_ARROW,
    }
}
