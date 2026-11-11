//! Direct2D / DirectWrite factories and a layered-window surface.

use std::ffi::c_void;
use windows::core::{w, Result};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT};
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dw: IDWriteFactory,
}

thread_local! {
    static GFX: Gfx = unsafe { Gfx::new() }.expect("Direct2D init failed");
}

pub fn with<R>(f: impl FnOnce(&Gfx) -> R) -> R {
    GFX.with(f)
}

impl Gfx {
    unsafe fn new() -> Result<Self> {
        Ok(Self {
            d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
            dw: DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?,
        })
    }

    pub unsafe fn text_format(&self, size: f32, bold: bool) -> Result<IDWriteTextFormat> {
        let weight = if bold { DWRITE_FONT_WEIGHT_SEMI_BOLD } else { DWRITE_FONT_WEIGHT_NORMAL };
        let fmt = self.dw.CreateTextFormat(
            w!("Microsoft YaHei UI"),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size,
            w!("zh-cn"),
        )?;
        fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(fmt)
    }

    /// Width of `text` laid out on one line in `fmt`.
    pub unsafe fn text_width(&self, text: &[u16], fmt: &IDWriteTextFormat) -> f32 {
        let Ok(layout) = self.dw.CreateTextLayout(text, fmt, 10_000.0, 1_000.0) else { return 0.0 };
        let mut m = DWRITE_TEXT_METRICS::default();
        if layout.GetMetrics(&mut m).is_err() {
            return 0.0;
        }
        m.widthIncludingTrailingWhitespace
    }

    /// Centered, wrapping label text that ends in "…" when it overflows its box.
    pub unsafe fn label_format(&self, size: f32) -> Result<IDWriteTextFormat> {
        let fmt = self.text_format(size, false)?;
        fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
        fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        fmt.SetWordWrapping(DWRITE_WORD_WRAPPING_EMERGENCY_BREAK)?;
        let sign = self.dw.CreateEllipsisTrimmingSign(&fmt)?;
        let trim = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        fmt.SetTrimming(&trim, &sign)?;
        Ok(fmt)
    }
}

pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

/// A 32-bit premultiplied DIB that a Direct2D DC render target draws into,
/// then pushed to a layered window with per-pixel alpha.
pub struct Surface {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut c_void,
    w: i32,
    h: i32,
    pub rt: ID2D1DCRenderTarget,
}

impl Surface {
    pub unsafe fn new() -> Result<Self> {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            // 96 DPI: one D2D unit == one physical pixel; callers scale explicitly.
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = with(|g| g.d2d.CreateDCRenderTarget(&props))?;
        Ok(Self { dc: CreateCompatibleDC(None), bmp: HBITMAP::default(), old: HGDIOBJ::default(), bits: std::ptr::null_mut(), w: 0, h: 0, rt })
    }

    unsafe fn ensure_size(&mut self, w: i32, h: i32) -> Result<()> {
        if w == self.w && h == self.h && !self.bmp.is_invalid() {
            return Ok(());
        }
        self.free_bitmap();
        let bmi = BITMAPINFO {
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
        let mut bits: *mut c_void = std::ptr::null_mut();
        self.bmp = CreateDIBSection(Some(self.dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)?;
        self.old = SelectObject(self.dc, self.bmp.into());
        self.bits = bits;
        self.w = w;
        self.h = h;
        Ok(())
    }

    unsafe fn free_bitmap(&mut self) {
        if !self.bmp.is_invalid() {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            self.bmp = HBITMAP::default();
        }
    }

    /// Draws a frame of `w`x`h` with `draw`, then places the window at `pos` with that size.
    pub unsafe fn present(
        &mut self,
        hwnd: HWND,
        pos: POINT,
        w: i32,
        h: i32,
        draw: impl FnOnce(&ID2D1DCRenderTarget) -> Result<()>,
    ) -> Result<()> {
        self.ensure_size(w, h)?;
        self.rt.BindDC(self.dc, &RECT { left: 0, top: 0, right: w, bottom: h })?;
        self.rt.BeginDraw();
        self.rt.Clear(Some(&rgba(0.0, 0.0, 0.0, 0.0)));
        let drawn = draw(&self.rt);
        self.rt.EndDraw(None, None)?;
        drawn?;
        if let Some(dir) = std::env::var_os("FENCES_DUMP") {
            let _ = GdiFlush();
            self.dump(std::path::Path::new(&dir).join(format!("{:x}.bmp", hwnd.0 as usize)));
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&pos),
            Some(&SIZE { cx: w, cy: h }),
            Some(self.dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    }
}

impl Surface {
    /// Debug aid: writes the last frame as a 32-bit BMP (premultiplied, i.e. as if over black).
    fn dump(&self, path: std::path::PathBuf) {
        let size = (self.w * self.h * 4) as usize;
        let pixels = unsafe { std::slice::from_raw_parts(self.bits as *const u8, size) };
        let mut out = Vec::with_capacity(54 + size);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(54 + size as u32).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&self.w.to_le_bytes());
        out.extend_from_slice(&(-self.h).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&[0; 24]);
        out.extend_from_slice(pixels);
        let _ = std::fs::write(path, out);
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            self.free_bitmap();
            let _ = DeleteDC(self.dc);
        }
    }
}
