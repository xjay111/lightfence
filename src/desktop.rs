//! The shell desktop: its host window (so fences can live on the desktop layer),
//! its icon view, the items it shows, and change notifications for them.

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use windows::core::{w, BOOL, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The top-level window that hosts `SHELLDLL_DefView` (the desktop icon view).
/// Usually `Progman`, but it moves to a `WorkerW` once wallpaper animations have run.
pub fn host() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).ok()?;
        if FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok() {
            return Some(progman);
        }
        let mut found = HWND::default();
        let _ = EnumWindows(Some(find_defview_host), LPARAM(&mut found as *mut HWND as isize));
        Some(if found.is_invalid() { progman } else { found })
    }
}

unsafe extern "system" fn find_defview_host(hwnd: HWND, lp: LPARAM) -> BOOL {
    if FindWindowExW(Some(hwnd), None, w!("SHELLDLL_DefView"), PCWSTR::null()).is_ok() {
        *(lp.0 as *mut HWND) = hwnd;
        return false.into();
    }
    true.into()
}

/// Explorer's own desktop icon list.
fn icon_view() -> Option<HWND> {
    unsafe {
        let defview = FindWindowExW(host(), None, w!("SHELLDLL_DefView"), PCWSTR::null()).ok()?;
        FindWindowExW(Some(defview), None, w!("SysListView32"), PCWSTR::null()).ok()
    }
}

/// Whether Explorer's desktop icons are currently shown.
pub fn icons_visible() -> bool {
    icon_view().is_some_and(|v| unsafe { IsWindowVisible(v) }.as_bool())
}

/// Shows or hides Explorer's desktop icons. Only the window is hidden, so a
/// restarted Explorer always comes back with its icons.
pub fn set_icons_visible(visible: bool) {
    if let Some(view) = icon_view() {
        unsafe {
            let _ = ShowWindowAsync(view, if visible { SW_SHOWNA } else { SW_HIDE });
        }
    }
}

fn known_folder(id: &windows::core::GUID) -> Option<String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

/// The user's own desktop folder (where new and pasted items go).
pub fn user_folder() -> Option<String> {
    folders().first().cloned()
}

/// The user's and the public desktop folders.
fn folders() -> &'static [String] {
    static FOLDERS: OnceLock<Vec<String>> = OnceLock::new();
    FOLDERS.get_or_init(|| [FOLDERID_Desktop, FOLDERID_PublicDesktop].iter().filter_map(known_folder).collect())
}

/// True if `path` sits directly in one of the desktop folders.
pub fn is_desktop_item(path: &str) -> bool {
    let parent = Path::new(path).parent().map(PathBuf::from).unwrap_or_default();
    let parent = parent.to_string_lossy().to_lowercase();
    folders().iter().any(|f| f.to_lowercase() == parent)
}

/// Virtual desktop icons (此电脑, 回收站 …) are stored as shell parsing names.
pub fn is_virtual(path: &str) -> bool {
    path.starts_with("::")
}

/// System icons the user enabled in "桌面图标设置", with their default visibility.
const SYSTEM_ICONS: [(&str, bool); 5] = [
    ("::{20D04FE0-3AEA-1069-A2D8-08002B30309D}", false), // 此电脑
    ("::{59031A47-3F72-44A7-89C5-5595FE6B30EE}", false), // 用户的文件
    ("::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}", false), // 网络
    ("::{645FF040-5081-101B-9F08-00AA002F954E}", true),  // 回收站
    ("::{5399E694-6CE5-4D6C-8FCE-1D8870FDCBA0}", false), // 控制面板
];

fn system_icon_shown(clsid: &str, default: bool) -> bool {
    let mut hidden = 0u32;
    let mut size = 4u32;
    let key = w!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel");
    let name = HSTRING::from(&clsid[2..]);
    let err = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, &name, RRF_RT_REG_DWORD, None, Some(&mut hidden as *mut u32 as _), Some(&mut size))
    };
    if err.is_ok() {
        hidden == 0
    } else {
        default
    }
}

/// Everything the desktop shows: enabled system icons, then the files in both
/// desktop folders (minus protected system files such as desktop.ini).
pub fn items() -> Vec<String> {
    let mut out: Vec<String> =
        SYSTEM_ICONS.iter().filter(|(c, d)| system_icon_shown(c, *d)).map(|(c, _)| c.to_string()).collect();
    for dir in folders() {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let attrs = e.metadata().map(|m| m.file_attributes()).unwrap_or(0);
            const HIDDEN_SYSTEM: u32 = 0x2 | 0x4;
            if attrs & HIDDEN_SYSTEM == HIDDEN_SYSTEM || e.file_name().eq_ignore_ascii_case("desktop.ini") {
                continue;
            }
            out.push(e.path().to_string_lossy().into_owned());
        }
    }
    out
}

/// Asks the shell to post `msg` to `hwnd` whenever something on the desktop is
/// created, deleted or renamed. Event-driven, so it costs nothing while idle.
pub fn watch(hwnd: HWND, msg: u32) -> u32 {
    let entries: Vec<SHChangeNotifyEntry> = [FOLDERID_Desktop, FOLDERID_PublicDesktop]
        .iter()
        .filter_map(|id| unsafe { SHGetKnownFolderIDList(id, 0, None) }.ok())
        .map(|pidl| SHChangeNotifyEntry { pidl, fRecursive: false.into() })
        .collect();
    let events = SHCNE_CREATE | SHCNE_DELETE | SHCNE_MKDIR | SHCNE_RMDIR | SHCNE_RENAMEITEM | SHCNE_RENAMEFOLDER | SHCNE_UPDATEDIR;
    let id = unsafe {
        SHChangeNotifyRegister(
            hwnd,
            SHCNRF_ShellLevel | SHCNRF_InterruptLevel | SHCNRF_NewDelivery,
            events.0 as i32,
            msg,
            entries.len() as i32,
            entries.as_ptr(),
        )
    };
    for e in entries {
        unsafe { CoTaskMemFree(Some(e.pidl as _)) };
    }
    id
}

/// For a change notification message: `Some((old, new))` if it reports a rename.
pub fn renamed(wp: WPARAM, lp: LPARAM) -> Option<(String, String)> {
    unsafe {
        let mut pidls: *mut *mut ITEMIDLIST = std::ptr::null_mut();
        let mut event = 0i32;
        let lock = SHChangeNotification_Lock(HANDLE(wp.0 as _), lp.0 as u32, Some(&mut pidls), Some(&mut event));
        if lock.is_invalid() {
            return None;
        }
        let rename = (SHCNE_RENAMEITEM | SHCNE_RENAMEFOLDER).0 as i32;
        let result = if event & rename != 0 && !pidls.is_null() {
            match (path_of(*pidls), path_of(*pidls.add(1))) {
                (Some(a), Some(b)) => Some((a, b)),
                _ => None,
            }
        } else {
            None
        };
        let _ = SHChangeNotification_Unlock(lock);
        result
    }
}

unsafe fn path_of(pidl: *const ITEMIDLIST) -> Option<String> {
    if pidl.is_null() {
        return None;
    }
    let mut buf = [0u16; 260];
    SHGetPathFromIDListW(pidl, &mut buf).as_bool().then(|| {
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..n])
    })
}
