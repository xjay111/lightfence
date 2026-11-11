//! Icons, thumbnails and display names from the shell.

use windows::core::{Interface, HSTRING};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::*;

/// 32-bit BGRA, premultiplied alpha, top-down rows.
pub struct Pixels {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// Display name as Explorer shows it (e.g. without `.lnk`).
pub fn display_name(path: &str) -> Option<String> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()?;
        let p = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as _));
        s
    }
}

/// The item's thumbnail (for images etc.) or icon, fitted into `size`x`size`.
pub fn image(path: &str, size: i32) -> Option<Pixels> {
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()?;
        let factory: IShellItemImageFactory = item.cast().ok()?;
        let bmp = factory.GetImage(SIZE { cx: size, cy: size }, SIIGBF_RESIZETOFIT).ok()?;
        let px = read_bitmap(bmp);
        let _ = DeleteObject(bmp.into());
        px
    }
}

unsafe fn read_bitmap(bmp: HBITMAP) -> Option<Pixels> {
    let mut info = BITMAP::default();
    if GetObjectW(bmp.into(), size_of::<BITMAP>() as i32, Some(&mut info as *mut _ as _)) == 0 {
        return None;
    }
    let (w, h) = (info.bmWidth, info.bmHeight.abs());
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut data = vec![0u8; (w * h * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(dc, bmp, 0, h as u32, Some(data.as_mut_ptr() as _), &mut bmi, DIB_RGB_COLORS);
    ReleaseDC(None, dc);
    if lines == 0 {
        return None;
    }
    if data.chunks_exact(4).all(|p| p[3] == 0) {
        // Opaque thumbnails often come back with an all-zero alpha channel.
        data.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    } else if data.chunks_exact(4).any(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3]) {
        // Straight alpha: Direct2D wants it premultiplied.
        for p in data.chunks_exact_mut(4) {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = (*c as u32 * a / 255) as u8;
            }
        }
    }
    Some(Pixels { w: w as u32, h: h as u32, data })
}
