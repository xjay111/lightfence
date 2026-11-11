//! Explorer behaviour for fence items: the shell's own context menu and dragging
//! items out with the standard drag image.

use std::cell::RefCell;
use windows::core::{Interface, HSTRING, PCSTR, PCWSTR, PSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_SHIFT};
use windows::Win32::UI::Shell::Common::{ITEMIDLIST, STRRET};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// From shobjidl.h; not exported by the windows crate.
const CMIC_MASK_UNICODE: u32 = 0x4000;
const FIRST_SHELL_ID: u32 = 1000;

/// Our own entries in an item's context menu.
pub const ID_REMOVE: u32 = 1;
pub const ID_LOCATE: u32 = 2;
pub const ID_OPEN: u32 = 3;
/// The shell's "重命名": we do the in-place edit ourselves.
pub const ID_RENAME: u32 = 4;

/// A shell context menu built ahead of time (on right-button down), because asking
/// every installed shell extension for its entries takes a noticeable moment.
struct Prepared {
    paths: Vec<String>,
    in_fence: bool,
    cm: IContextMenu,
    menu: HMENU,
}

impl Drop for Prepared {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.menu);
        }
    }
}

thread_local! {
    static PREPARED: RefCell<Option<Prepared>> = const { RefCell::new(None) };
    /// The shell menu being tracked, so its owner window can forward owner-draw
    /// messages ("Open with" icons, "Send to" submenu contents).
    static ACTIVE: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
    /// Items currently being dragged out of a fence (the grabbed one first).
    /// Virtual items (回收站…) carry no file path in the data object, so drop
    /// targets look here first.
    static DRAGGING: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

pub fn dragging() -> Option<Vec<String>> {
    DRAGGING.with_borrow(|d| d.clone())
}

/// Child ID lists of the shell's Desktop folder, which merges the user and public
/// desktop folders with 此电脑/回收站, so any mix of desktop items can share one
/// context menu or data object.
struct DesktopItems {
    folder: IShellFolder,
    pidls: Vec<*mut ITEMIDLIST>,
}

impl DesktopItems {
    fn find(paths: &[String]) -> Option<Self> {
        unsafe {
            let folder = SHGetDesktopFolder().ok()?;
            let mut list = None;
            let flags = (SHCONTF_FOLDERS.0 | SHCONTF_NONFOLDERS.0 | SHCONTF_INCLUDEHIDDEN.0) as u32;
            folder.EnumObjects(HWND::default(), flags, &mut list).ok().ok()?;
            let list = list?;
            let wanted: Vec<String> = paths.iter().map(|p| p.to_lowercase()).collect();
            let mut found: Vec<Option<*mut ITEMIDLIST>> = vec![None; paths.len()];
            let mut pidl = [std::ptr::null_mut()];
            while list.Next(&mut pidl, None).is_ok() && !pidl[0].is_null() {
                let name = parsing_name(&folder, pidl[0]).map(|n| n.to_lowercase());
                match name.and_then(|n| wanted.iter().position(|w| *w == n)) {
                    Some(i) if found[i].is_none() => found[i] = Some(pidl[0]),
                    _ => CoTaskMemFree(Some(pidl[0] as _)),
                }
                pidl[0] = std::ptr::null_mut();
            }
            let pidls: Vec<_> = found.into_iter().flatten().collect();
            (!pidls.is_empty()).then_some(Self { folder, pidls })
        }
    }

    fn ui_object<T: Interface>(&self) -> Option<T> {
        let apidl: Vec<*const ITEMIDLIST> = self.pidls.iter().map(|p| *p as *const _).collect();
        unsafe { self.folder.GetUIObjectOf(HWND::default(), &apidl, None).ok() }
    }
}

impl Drop for DesktopItems {
    fn drop(&mut self) {
        for p in &self.pidls {
            unsafe { CoTaskMemFree(Some(*p as _)) };
        }
    }
}

unsafe fn parsing_name(folder: &IShellFolder, pidl: *const ITEMIDLIST) -> Option<String> {
    let mut sr = STRRET::default();
    folder.GetDisplayNameOf(pidl, SHGDN_FORPARSING, &mut sr).ok()?;
    let mut out = PWSTR::null();
    StrRetToStrW(&mut sr, Some(pidl), &mut out).ok()?;
    let s = out.to_string().ok();
    CoTaskMemFree(Some(out.0 as _));
    s
}

fn verb(cm: &IContextMenu, id: u32) -> String {
    let mut buf = [0u16; 64];
    let ok = unsafe { cm.GetCommandString((id - FIRST_SHELL_ID) as usize, GCS_VERBW, None, PSTR(buf.as_mut_ptr() as _), 64) };
    if ok.is_err() {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)])
}

fn is_separator(menu: HMENU, pos: i32) -> bool {
    unsafe { GetMenuState(menu, pos as u32, MF_BYPOSITION) & MF_SEPARATOR.0 != 0 }
}

/// Removes leading, trailing and doubled separators.
fn tidy_separators(menu: HMENU) {
    unsafe {
        let mut i = GetMenuItemCount(Some(menu)) - 1;
        while i >= 0 {
            let count = GetMenuItemCount(Some(menu));
            if is_separator(menu, i) && (i == 0 || i == count - 1 || is_separator(menu, i - 1)) {
                let _ = DeleteMenu(menu, i as u32, MF_BYPOSITION);
            }
            i -= 1;
        }
    }
}

/// Runs shell menu command `cmd` (an id from `QueryContextMenu`).
unsafe fn invoke_cmd(cm: &IContextMenu, owner: HWND, cmd: u32, pt: POINT) {
    let verb = (cmd - FIRST_SHELL_ID) as usize; // MAKEINTRESOURCE
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        lpVerb: PCSTR(verb as *const u8),
        lpVerbW: PCWSTR(verb as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: pt,
        ..Default::default()
    };
    if let Err(e) = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) {
        crate::log(&format!("context menu command failed: {e}"));
    }
}

/// Shows `menu` for shell menu `cm` and returns the chosen id (0 if dismissed),
/// forwarding owner-draw messages to the shell while it is open.
unsafe fn track(owner: HWND, cm: &IContextMenu, menu: HMENU, pt: POINT) -> u32 {
    ACTIVE.set(Some(cm.clone()));
    crate::take_foreground(owner);
    let cmd = TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, owner, None).0 as u32;
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    ACTIVE.set(None);
    cmd
}

pub enum BgChoice {
    Dismissed,
    /// One of the caller's own entries.
    Ours(u32),
    /// A shell command ran (it may create a new item on the desktop).
    Shell,
}

/// Explorer's desktop background menu ("新建", "显示设置", "个性化" and items
/// other apps register), with the caller's entries added via `top` and `bottom`.
/// Caller ids must be below 1000.
pub fn background_menu(owner: HWND, pt: POINT, top: impl FnOnce(HMENU), bottom: impl FnOnce(HMENU)) -> BgChoice {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return BgChoice::Dismissed };
        top(menu);
        let cm = SHGetDesktopFolder().and_then(|d| d.CreateViewObject::<IContextMenu>(HWND::default())).ok();
        if let Some(cm) = &cm {
            let at = GetMenuItemCount(Some(menu)).max(0) as u32;
            let _ = InsertMenuW(menu, at, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
            let _ = cm.QueryContextMenu(menu, at + 1, FIRST_SHELL_ID, 0x7FFF, CMF_NORMAL);
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        bottom(menu);
        tidy_separators(menu);
        let cmd = match &cm {
            Some(cm) => track(owner, cm, menu, pt),
            None => {
                crate::take_foreground(owner);
                TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, owner, None).0 as u32
            }
        };
        let choice = match (cmd, &cm) {
            (0, _) => BgChoice::Dismissed,
            (c, _) if c < FIRST_SHELL_ID => BgChoice::Ours(c),
            (c, Some(cm)) => {
                invoke_cmd(cm, owner, c, pt);
                BgChoice::Shell
            }
            _ => BgChoice::Dismissed,
        };
        let _ = DestroyMenu(menu);
        choice
    }
}

/// Files on the clipboard that "粘贴" could put on the desktop.
pub fn can_paste() -> bool {
    unsafe { windows::Win32::System::DataExchange::IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() }
}

/// Pastes the clipboard's files (or shortcuts to them, if `link`) onto the desktop.
pub fn paste_to_desktop(owner: HWND, link: bool) {
    let Some(dir) = crate::desktop::user_folder() else { return };
    let cm = unsafe { SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir), None) }
        .and_then(|i| unsafe { i.BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject) });
    if let Ok(cm) = cm {
        invoke_verb_on(&cm, owner, if link { "pastelink" } else { "paste" });
    }
}

/// Builds Explorer's menu for `paths` (the clicked item first), reshaped to: open,
/// open file location, remove from fence (if `in_fence`), then the shell entries.
fn build(paths: &[String], in_fence: bool) -> Option<Prepared> {
    unsafe {
        let cm: IContextMenu = DesktopItems::find(paths)?.ui_object()?;
        let menu = CreatePopupMenu().ok()?;
        // CANRENAME: we handle "重命名" ourselves (the shell would ask a view we don't have).
        let mut flags = CMF_NORMAL | CMF_CANRENAME;
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            flags |= CMF_EXTENDEDVERBS;
        }
        let _ = cm.QueryContextMenu(menu, 0, FIRST_SHELL_ID, 0x7FFF, flags);

        // Shortcuts already have "open file location" (it opens the target's folder);
        // pull it up into our slot instead of listing it twice.
        let mut locate = ID_LOCATE;
        for pos in (0..GetMenuItemCount(Some(menu))).rev() {
            let id = GetMenuItemID(menu, pos);
            if id != u32::MAX && id >= FIRST_SHELL_ID && verb(&cm, id) == "opencontaining" {
                locate = id;
                let _ = DeleteMenu(menu, pos as u32, MF_BYPOSITION);
            }
        }

        // Keep the shell's own default "打开" on top; add ours if it isn't there.
        if GetMenuDefaultItem(menu, 1, GET_MENU_DEFAULT_ITEM_FLAGS(0)) != 0 {
            let _ = InsertMenuW(menu, 0, MF_BYPOSITION | MF_STRING, ID_OPEN as usize, &HSTRING::from("打开"));
        }
        let mut pos = 1;
        if paths.len() == 1 && !crate::desktop::is_virtual(&paths[0]) {
            let _ = InsertMenuW(menu, pos, MF_BYPOSITION | MF_STRING, locate as usize, &HSTRING::from("打开文件所在位置"));
            pos += 1;
        }
        if in_fence {
            let _ = InsertMenuW(menu, pos, MF_BYPOSITION | MF_STRING, ID_REMOVE as usize, &HSTRING::from("移出分区"));
            pos += 1;
        }
        let _ = InsertMenuW(menu, pos, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());

        // Removing entries can leave separators doubled up or dangling at the end.
        tidy_separators(menu);
        Some(Prepared { paths: paths.to_vec(), in_fence, cm, menu })
    }
}

/// Builds the menu for `paths` now so the right-button release can show it at once.
pub fn prepare(paths: &[String], in_fence: bool) {
    let prepared = build(paths, in_fence);
    PREPARED.set(prepared);
}

/// Shows the item menu for `paths`. Shell commands run here; returns our own
/// entry (`ID_*`) if one was chosen.
pub fn context_menu(owner: HWND, paths: &[String], in_fence: bool, pt: POINT) -> Option<u32> {
    let prepared = PREPARED
        .take()
        .filter(|p| p.paths == paths && p.in_fence == in_fence)
        .or_else(|| build(paths, in_fence))?;
    unsafe {
        let cmd = track(owner, &prepared.cm, prepared.menu, pt);

        if cmd < FIRST_SHELL_ID {
            return (cmd != 0).then_some(cmd);
        }
        if verb(&prepared.cm, cmd) == "rename" {
            return Some(ID_RENAME);
        }
        invoke_cmd(&prepared.cm, owner, cmd, pt);
        None
    }
}

/// Runs a canonical shell verb ("delete", "properties"…) on `paths`, as if picked
/// from their context menu. Shift held turns "delete" into a permanent delete.
pub fn invoke_verb(owner: HWND, paths: &[String], verb: &str) {
    if let Some(cm) = DesktopItems::find(paths).and_then(|d| d.ui_object::<IContextMenu>()) {
        invoke_verb_on(&cm, owner, verb);
    }
}

fn invoke_verb_on(cm: &IContextMenu, owner: HWND, verb: &str) {
    let ansi: Vec<u8> = verb.bytes().chain([0]).collect();
    let wide: Vec<u16> = verb.encode_utf16().chain([0]).collect();
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let _ = cm.QueryContextMenu(menu, 0, FIRST_SHELL_ID, 0x7FFF, CMF_NORMAL);
        let mut mask = CMIC_MASK_UNICODE;
        if GetKeyState(VK_SHIFT.0 as i32) < 0 {
            mask |= CMIC_MASK_SHIFT_DOWN;
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: mask,
            hwnd: owner,
            lpVerb: PCSTR(ansi.as_ptr()),
            lpVerbW: PCWSTR(wide.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        if let Err(e) = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) {
            crate::log(&format!("{verb} failed: {e}"));
        }
        let _ = DestroyMenu(menu);
    }
}

/// The name Explorer shows when renaming `path` (keeps the extension only if
/// extensions are visible).
pub fn editing_name(path: &str) -> Option<String> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()?;
        let p = item.GetDisplayName(SIGDN_PARENTRELATIVEEDITING).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

/// Renames `path` the way Explorer does (undoable, keeps a hidden extension).
pub fn rename(owner: HWND, path: &str, new_name: &str) {
    unsafe {
        let r: windows::core::Result<()> = (|| {
            let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
            let op: IFileOperation = windows::Win32::System::Com::CoCreateInstance(&FileOperation, None, windows::Win32::System::Com::CLSCTX_ALL)?;
            op.SetOwnerWindow(owner)?;
            op.SetOperationFlags(FOF_ALLOWUNDO | FOFX_ADDUNDORECORD)?;
            op.RenameItem(&item, &HSTRING::from(new_name), None)?;
            op.PerformOperations()
        })();
        if let Err(e) = r {
            crate::log(&format!("rename failed: {e}"));
        }
    }
}

/// Lets the active shell menu handle owner-draw/submenu messages sent to its owner.
pub fn forward_menu_msg(msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
    if !matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        return None;
    }
    let cm = ACTIVE.with_borrow(|c| c.clone())?;
    unsafe {
        if let Ok(cm3) = cm.cast::<IContextMenu3>() {
            let mut r = LRESULT(0);
            return cm3.HandleMenuMsg2(msg, wp, lp, Some(&mut r)).is_ok().then_some(r);
        }
        let cm2 = cm.cast::<IContextMenu2>().ok()?;
        cm2.HandleMenuMsg(msg, wp, lp).is_ok().then_some(LRESULT(0))
    }
}

/// Drags `paths` (the grabbed item first) exactly like Explorer would; returns
/// the effect the drop target performed.
pub fn drag_out(hwnd: HWND, paths: &[String]) -> DROPEFFECT {
    let Some(data) = DesktopItems::find(paths).and_then(|d| d.ui_object::<IDataObject>()) else {
        return DROPEFFECT_NONE;
    };
    DRAGGING.set(Some(paths.to_vec()));
    let effect = unsafe { SHDoDragDrop(Some(hwnd), &data, None::<&IDropSource>, DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK) }
        .unwrap_or(DROPEFFECT_NONE);
    DRAGGING.set(None);
    effect
}
