//! Drops onto a fence or the desktop layer.
//!
//! Desktop items (from the desktop, Explorer's desktop folder, or another fence)
//! are only re-filed: we record them here and never touch the files. Anything
//! else — files from WeChat, QQ, other folders, images from a browser — goes to
//! the desktop folder's own shell drop target, which copies or moves it exactly
//! like a drop on Explorer's desktop; the new items are then placed where the
//! user let go.

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HWND, POINT, POINTL};
use windows::Win32::System::Com::*;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::*;
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::*;
use windows_core::{implement, Ref, Result};

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Reject,
    /// Desktop items: re-file them, leave the files alone.
    Refile,
    /// Anything else: let the shell put it on the desktop.
    Import,
}

#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
    mode: Cell<Mode>,
    /// Draws the shell drag image (and our caption) over the fence.
    helper: Option<IDropTargetHelper>,
    /// The desktop folder's drop target while importing.
    shell: RefCell<Option<IDropTarget>>,
    data: RefCell<Option<IDataObject>>,
    /// Effect the caption was last set for, to avoid rewriting it on every move.
    shown: Cell<DROPEFFECT>,
}

impl DropTarget {
    pub fn new(hwnd: HWND) -> IDropTarget {
        let helper = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.ok();
        Self {
            hwnd,
            mode: Cell::new(Mode::Reject),
            helper,
            shell: RefCell::new(None),
            data: RefCell::new(None),
            shown: Cell::new(DROPEFFECT(u32::MAX)),
        }
        .into()
    }

    /// Asks the shell (when importing) what a drop here would do; `allowed` is
    /// what the source offers. Re-filing is always a COPY so the source never
    /// deletes anything.
    unsafe fn effect(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, allowed: DROPEFFECT) -> DROPEFFECT {
        match self.mode.get() {
            Mode::Reject => DROPEFFECT_NONE,
            Mode::Refile => DROPEFFECT_COPY,
            Mode::Import => {
                let mut effect = allowed;
                match self.shell.borrow().as_ref() {
                    Some(t) if t.DragOver(keys, *pt, &mut effect).is_ok() => effect,
                    _ => DROPEFFECT_NONE,
                }
            }
        }
    }

    /// Caption under the drag image: "移动到 / 复制到 <fence>".
    fn describe(&self, effect: DROPEFFECT) {
        if effect == self.shown.get() {
            return;
        }
        self.shown.set(effect);
        let Some(data) = self.data.borrow().clone() else { return };
        let name = crate::fence::with(self.hwnd, |f| f.cfg.tab().name.clone()).unwrap_or_default();
        let (kind, msg) = if self.mode.get() == Mode::Refile || effect.0 & DROPEFFECT_MOVE.0 != 0 {
            (DROPIMAGE_MOVE, "移动到 %1")
        } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
            (DROPIMAGE_COPY, "复制到 %1")
        } else if effect.0 & DROPEFFECT_LINK.0 != 0 {
            (DROPIMAGE_LINK, "在 %1 创建快捷方式")
        } else {
            (DROPIMAGE_INVALID, "")
        };
        set_description(&data, kind, msg, &name);
    }

    fn reset(&self) {
        crate::fence::drag_over(self.hwnd, None);
        if let Some(t) = self.shell.take() {
            unsafe {
                let _ = t.DragLeave();
            }
        }
        if let Some(d) = self.data.take() {
            set_description(&d, DROPIMAGE_INVALID, "", "");
        }
        self.mode.set(Mode::Reject);
        self.shown.set(DROPEFFECT(u32::MAX));
    }
}

fn point(pt: &POINTL) -> POINT {
    POINT { x: pt.x, y: pt.y }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let data = data.as_ref();
        let allowed = unsafe { *effect };
        let mut mode = Mode::Reject;
        if let Some(d) = data {
            let paths = dropped_paths(d);
            if !paths.is_empty() && paths.iter().all(|p| crate::desktop::is_desktop_item(p)) {
                mode = Mode::Refile;
            } else if let Some(t) = crate::shell::desktop_drop_target() {
                let mut e = allowed;
                if unsafe { t.DragEnter(d, keys, *pt, &mut e) }.is_ok() && e != DROPEFFECT_NONE {
                    *self.shell.borrow_mut() = Some(t);
                    mode = Mode::Import;
                } else {
                    unsafe {
                        let _ = t.DragLeave();
                    }
                }
            }
        }
        self.mode.set(mode);
        *self.data.borrow_mut() = data.cloned();
        let e = unsafe { self.effect(keys, pt, allowed) };
        self.describe(e);
        crate::fence::drag_over(self.hwnd, (mode != Mode::Reject).then(|| point(pt)));
        unsafe {
            *effect = e;
            if let (Some(h), Some(d)) = (&self.helper, data) {
                let _ = h.DragEnter(self.hwnd, d, &point(pt), e);
            }
        }
        Ok(())
    }

    fn DragOver(&self, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let e = unsafe { self.effect(keys, pt, *effect) };
        if self.mode.get() != Mode::Reject {
            self.describe(e);
            crate::fence::drag_over(self.hwnd, Some(point(pt)));
        }
        unsafe {
            *effect = e;
            if let Some(h) = &self.helper {
                let _ = h.DragOver(&point(pt), e);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        self.reset();
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragLeave();
            }
        }
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, keys: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let data = data.as_ref();
        let allowed = unsafe { *effect };
        let mut result = DROPEFFECT_NONE;
        match (self.mode.get(), data) {
            (Mode::Refile, Some(d)) => {
                let paths: Vec<String> = dropped_paths(d).into_iter().filter(|p| crate::desktop::is_desktop_item(p)).collect();
                if !paths.is_empty() {
                    result = DROPEFFECT_COPY;
                    crate::add_items(self.hwnd, paths, point(pt));
                }
            }
            (Mode::Import, Some(d)) => {
                if let Some(t) = self.shell.take() {
                    // The new desktop items show up through the desktop watcher; put them here.
                    crate::expect_new_items(Some(self.hwnd), point(pt), false);
                    let mut e = allowed;
                    if unsafe { t.Drop(d, keys, *pt, &mut e) }.is_ok() {
                        result = e;
                    }
                }
            }
            _ => {}
        }
        unsafe {
            *effect = result;
            if let (Some(h), Some(d)) = (&self.helper, data) {
                let _ = h.Drop(d, &point(pt), result);
            }
        }
        self.reset();
        Ok(())
    }
}

/// File paths in the dragged data (the grabbed item first when it comes from a fence).
fn dropped_paths(data: &IDataObject) -> Vec<String> {
    if let Some(paths) = crate::shell::dragging() {
        return paths;
    }
    let fmt = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut paths = Vec::new();
    unsafe {
        let Ok(mut medium) = data.GetData(&fmt) else { return paths };
        let hdrop = HDROP(medium.u.hGlobal.0);
        for i in 0..DragQueryFileW(hdrop, u32::MAX, None) {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            DragQueryFileW(hdrop, i, Some(&mut buf));
            paths.push(String::from_utf16_lossy(&buf[..len]));
        }
        ReleaseStgMedium(&mut medium);
    }
    paths
}

/// Sets the caption under the shell drag image; `%1` in `msg` is replaced by `insert`.
fn set_description(data: &IDataObject, kind: DROPIMAGETYPE, msg: &str, insert: &str) {
    // DROPDESCRIPTION is packed, so fill the strings before building it.
    let wide = |s: &str| {
        let mut buf = [0u16; 260];
        buf.iter_mut().take(259).zip(s.encode_utf16()).for_each(|(d, c)| *d = c);
        buf
    };
    let desc = DROPDESCRIPTION { r#type: kind, szMessage: wide(msg), szInsert: wide(insert) };
    unsafe {
        let fmt = FORMATETC {
            cfFormat: RegisterClipboardFormatW(w!("DropDescription")) as u16,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, size_of::<DROPDESCRIPTION>()) else { return };
        (GlobalLock(mem) as *mut DROPDESCRIPTION).write(desc);
        let _ = GlobalUnlock(mem);
        let medium = STGMEDIUM { tymed: TYMED_HGLOBAL.0 as u32, u: STGMEDIUM_0 { hGlobal: mem }, pUnkForRelease: ManuallyDrop::new(None) };
        if data.SetData(&fmt, &medium, true).is_err() {
            let _ = GlobalFree(Some(mem));
        }
    }
}
