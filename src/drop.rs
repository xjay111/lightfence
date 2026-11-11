//! Accepting desktop items dragged onto a fence, from the desktop or another fence.

use std::cell::Cell;
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

#[implement(IDropTarget)]
pub struct DropTarget {
    hwnd: HWND,
    accept: Cell<bool>,
    /// Draws the shell drag image (and our "移动到 …" caption) over the fence.
    helper: Option<IDropTargetHelper>,
}

impl DropTarget {
    pub fn new(hwnd: HWND) -> IDropTarget {
        let helper = unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER) }.ok();
        Self { hwnd, accept: Cell::new(false), helper }.into()
    }

    fn effect(&self) -> DROPEFFECT {
        // COPY, never MOVE: the source must not delete anything. We only record paths.
        if self.accept.get() {
            DROPEFFECT_COPY
        } else {
            DROPEFFECT_NONE
        }
    }
}

fn point(pt: &POINTL) -> POINT {
    POINT { x: pt.x, y: pt.y }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(&self, data: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        let data = data.as_ref();
        let ok = data.is_some_and(|d| !desktop_paths(d).is_empty());
        self.accept.set(ok);
        if let (true, Some(d)) = (ok, data) {
            let title = crate::fence::with(self.hwnd, |f| f.cfg.tab().name.clone()).unwrap_or_default();
            set_description(d, DROPIMAGE_MOVE, "移动到 %1", &title);
        }
        crate::fence::drag_over(self.hwnd, ok.then(|| point(pt)));
        unsafe {
            *effect = self.effect();
            if let (Some(h), Some(d)) = (&self.helper, data) {
                let _ = h.DragEnter(self.hwnd, d, &point(pt), *effect);
            }
        }
        Ok(())
    }

    fn DragOver(&self, _: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        if self.accept.get() {
            crate::fence::drag_over(self.hwnd, Some(point(pt)));
        }
        unsafe {
            *effect = self.effect();
            if let Some(h) = &self.helper {
                let _ = h.DragOver(&point(pt), *effect);
            }
        }
        Ok(())
    }

    fn DragLeave(&self) -> Result<()> {
        crate::fence::drag_over(self.hwnd, None);
        if let Some(h) = &self.helper {
            unsafe {
                let _ = h.DragLeave();
            }
        }
        Ok(())
    }

    fn Drop(&self, data: Ref<IDataObject>, _: MODIFIERKEYS_FLAGS, pt: &POINTL, effect: *mut DROPEFFECT) -> Result<()> {
        crate::fence::drag_over(self.hwnd, None);
        let data = data.as_ref();
        let paths = data.map(desktop_paths).unwrap_or_default();
        self.accept.set(!paths.is_empty());
        unsafe {
            *effect = self.effect();
            if let (Some(h), Some(d)) = (&self.helper, data) {
                let _ = h.Drop(d, &point(pt), *effect);
            }
        }
        if let Some(d) = data {
            set_description(d, DROPIMAGE_INVALID, "", "");
        }
        if !paths.is_empty() {
            crate::add_items(self.hwnd, paths, point(pt));
        }
        Ok(())
    }
}

/// Dropped file paths that live directly on the desktop.
fn desktop_paths(data: &IDataObject) -> Vec<String> {
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
    paths.retain(|p| crate::desktop::is_desktop_item(p));
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
