//! Current cursor bitmap only, guarded by the captured target's root and physical rectangle.
//! GetIconInfoEx does not DPI-virtualize its device-pixel bitmap; WGC and DWM extended frame
//! bounds also use physical pixels. Thus matching extents give density 1:1, with no second DPI
//! multiplication. High/mixed-DPI behavior still needs attended validation.
#![allow(unsafe_code)]

use crate::{
    model::cursor::{Cache, History, Mask, Rows, Shape, convert, over_content},
    window::NativeWindow,
};
use crosspane_types::geom::{PixelRect, PixelSize};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    mem::size_of,
    ptr::null_mut,
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::{Dwm::*, Gdi::*},
    UI::{HiDpi::*, WindowsAndMessaging::*},
};

struct Dpi(DPI_AWARENESS_CONTEXT);
impl Dpi {
    fn enter() -> Option<Self> {
        // SAFETY: changes only this capture worker's context and restores it before returning.
        let previous =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        (!previous.is_null()).then_some(Self(previous))
    }
}
impl Drop for Dpi {
    fn drop(&mut self) {
        // SAFETY: same thread, previously returned valid DPI context.
        unsafe { SetThreadDpiAwarenessContext(self.0) };
    }
}
struct Icon(HICON);
impl Drop for Icon {
    fn drop(&mut self) {
        // SAFETY: only CopyIcon's independently owned copy, never a borrowed system cursor.
        unsafe { DestroyIcon(self.0) };
    }
}
struct Info(ICONINFOEXW);
impl Drop for Info {
    fn drop(&mut self) {
        for bitmap in [self.0.hbmColor, self.0.hbmMask] {
            if !bitmap.is_null() {
                // SAFETY: GetIconInfoEx allocates these separate bitmaps; never selected in a DC.
                unsafe { DeleteObject(bitmap) };
            }
        }
    }
}
struct Dc(HDC);
impl Drop for Dc {
    fn drop(&mut self) {
        // SAFETY: only this read's private compatible memory DC.
        unsafe { DeleteDC(self.0) };
    }
}
#[repr(C)]
struct MaskInfo {
    header: BITMAPINFOHEADER,
    colours: [RGBQUAD; 2],
}

fn dimensions(bitmap: HBITMAP) -> Option<PixelSize> {
    let mut facts = BITMAP::default();
    // SAFETY: query our owned GetIconInfoEx bitmap into the exact fixed-size structure.
    let read = unsafe {
        GetObjectW(
            bitmap,
            size_of::<BITMAP>() as i32,
            (&mut facts as *mut BITMAP).cast(),
        )
    };
    if read != size_of::<BITMAP>() as i32
        || facts.bmWidth <= 0
        || facts.bmHeight <= 0
        || facts.bmWidth > 4096
        || facts.bmHeight > 8192
        || facts.bmPlanes != 1
    {
        return None;
    }
    Some(PixelSize::new(facts.bmWidth as u32, facts.bmHeight as u32))
}
fn bits(dc: HDC, bitmap: HBITMAP, size: PixelSize, mono: bool) -> Option<(usize, Vec<u8>)> {
    let stride = if mono {
        size.width.div_ceil(32) as usize * 4
    } else {
        size.width as usize * 4
    };
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(stride.checked_mul(size.height as usize)?)
        .ok()?;
    pixels.resize(stride * size.height as usize, 0);
    let mut info = MaskInfo {
        header: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size.width as i32,
            biHeight: -(size.height as i32), // Explicit top-down rows, including both mono masks.
            biPlanes: 1,
            biBitCount: if mono { 1 } else { 32 },
            biCompression: BI_RGB,
            biClrUsed: if mono { 2 } else { 0 },
            ..Default::default()
        },
        colours: [
            RGBQUAD::default(),
            RGBQUAD {
                rgbBlue: 255,
                rgbGreen: 255,
                rgbRed: 255,
                rgbReserved: 0,
            },
        ],
    };
    // SAFETY: owned bitmap not selected in any DC; initialized header and TWO palette entries
    // for 1bpp, checked full-row allocation for the requested bounded top-down format.
    let rows = unsafe {
        GetDIBits(
            dc,
            bitmap,
            0,
            size.height,
            pixels.as_mut_ptr().cast(),
            (&mut info as *mut MaskInfo).cast(),
            DIB_RGB_COLORS,
        )
    };
    (rows == size.height as i32).then_some((stride, pixels))
}

#[derive(Default)]
pub(crate) struct StreamCursor {
    history: History,
    cache: Cache,
}
impl StreamCursor {
    fn image(&mut self, handle: HCURSOR) -> Option<crosspane_platform::CursorImage> {
        // SAFETY: copy the borrowed current cursor; subsequent reads use only this owned copy.
        let copied = unsafe { CopyIcon(handle) };
        if copied.is_null() {
            return None;
        }
        let icon = Icon(copied);
        let mut info = Info(ICONINFOEXW {
            cbSize: size_of::<ICONINFOEXW>() as u32,
            ..Default::default()
        });
        // SAFETY: exact initialized output; Info drops any allocated bitmaps even on refusal.
        if unsafe { GetIconInfoExW(icon.0, &mut info.0) } == 0
            || info.0.fIcon != 0
            || info.0.hbmMask.is_null()
        {
            return None;
        }
        let mask_size = dimensions(info.0.hbmMask)?;
        let size = if info.0.hbmColor.is_null() {
            if mask_size.height % 2 != 0 {
                return None;
            }
            PixelSize::new(mask_size.width, mask_size.height / 2)
        } else {
            dimensions(info.0.hbmColor)?
        };
        if size.height > 4096 {
            return None;
        }
        // SAFETY: creates a private memory DC; never borrows a window/screen DC or selects bitmap.
        let created = unsafe { CreateCompatibleDC(null_mut()) };
        if created.is_null() {
            return None;
        }
        let dc = Dc(created);
        let (mask_stride, mask) = bits(dc.0, info.0.hbmMask, mask_size, true)?;
        let colour = if info.0.hbmColor.is_null() {
            None
        } else {
            Some(bits(dc.0, info.0.hbmColor, size, false)?)
        };
        let hotspot = (info.0.xHotspot, info.0.yHotspot);
        let mut hash = DefaultHasher::new();
        size.hash(&mut hash);
        mask_size.hash(&mut hash);
        hotspot.hash(&mut hash);
        mask.hash(&mut hash);
        colour.hash(&mut hash);
        self.cache.read(handle as usize, hash.finish(), || {
            convert(
                size,
                hotspot,
                colour.as_ref().map(|(stride, pixels)| Rows {
                    stride: *stride,
                    pixels,
                }),
                Mask {
                    size: mask_size,
                    rows: Rows {
                        stride: mask_stride,
                        pixels: &mask,
                    },
                },
                (1, 1),
            )
        })
    }

    /// No cursor failure escapes to the frame stream. Bounds/position failure skips entirely;
    /// a bitmap failure becomes Default only after confirming the pointer remains over target.
    pub(crate) fn sample(
        &mut self,
        target: NativeWindow,
        size: PixelSize,
        crop: Option<PixelRect>,
        permitted: impl Fn() -> bool,
    ) -> Option<Shape> {
        if !permitted() {
            return None;
        }
        let _dpi = Dpi::enter()?;
        let hwnd = target.hwnd as usize as HWND;
        let mut rect = RECT::default();
        // SAFETY: only the caller's freshly resolved admitted target's geometry is queried.
        if unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut rect as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            )
        } < 0
        {
            return None;
        }
        let bounds = [rect.left, rect.top, rect.right, rect.bottom];
        let over = |info: &CURSORINFO| {
            // SAFETY: allowed guard metadata only: no fields queried from a foreign root.
            let root = unsafe { GetAncestor(WindowFromPoint(info.ptScreenPos), GA_ROOT) };
            over_content(
                (info.ptScreenPos.x, info.ptScreenPos.y),
                bounds,
                size,
                crop,
                root == hwnd,
            )
        };
        let mut before = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: public current cursor metadata into exact initialized structure.
        if unsafe { GetCursorInfo(&mut before) } == 0 {
            return None;
        }
        if !over(&before) {
            return self.history.observe(false, Shape::Default);
        }
        if !permitted() {
            return None;
        }
        let shape = if before.flags & CURSOR_SHOWING == 0 {
            Shape::Hidden
        } else {
            self.image(before.hCursor)
                .map(Shape::Image)
                .unwrap_or(Shape::Default)
        };
        let mut after = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: public guard metadata rechecked after the owned bitmap read, before delivery.
        if unsafe { GetCursorInfo(&mut after) } == 0 || !permitted() {
            return None;
        }
        if !over(&after) {
            return self.history.observe(false, Shape::Default);
        }
        let mut current = RECT::default();
        // SAFETY: admitted target geometry only; reject motion/resize during the bitmap read.
        if unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&mut current as *mut RECT).cast(),
                size_of::<RECT>() as u32,
            )
        } < 0
            || [current.left, current.top, current.right, current.bottom] != bounds
        {
            return None;
        }
        if after.hCursor != before.hCursor || after.flags != before.flags {
            return None;
        }
        self.history.observe(true, shape)
    }

    /// Monitor capture has no HWND/root guard. Fresh retained monitor geometry and
    /// stream permission are supplied by the caller; bitmap reads occur only inside
    /// the physical captured area, with the same post-read checks as window capture.
    pub(crate) fn sample_monitor(
        &mut self,
        bounds: PixelRect,
        size: PixelSize,
        crop: Option<PixelRect>,
        permitted: impl Fn() -> bool,
    ) -> Option<Shape> {
        if !permitted() {
            return None;
        }
        let _dpi = Dpi::enter()?;
        let over = |info: &CURSORINFO| {
            over_content(
                (info.ptScreenPos.x, info.ptScreenPos.y),
                [bounds.min.x, bounds.min.y, bounds.max.x, bounds.max.y],
                size,
                crop,
                true,
            )
        };
        let mut before = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: only public current cursor metadata, no window fields or bitmap yet.
        if unsafe { GetCursorInfo(&mut before) } == 0 {
            return None;
        }
        if !over(&before) {
            return self.history.observe(false, Shape::Default);
        }
        if !permitted() {
            return None;
        }
        let shape = if before.flags & CURSOR_SHOWING == 0 {
            Shape::Hidden
        } else {
            self.image(before.hCursor)
                .map(Shape::Image)
                .unwrap_or(Shape::Default)
        };
        let mut after = CURSORINFO {
            cbSize: size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: recheck public cursor guards after the owned copy, before delivery.
        if unsafe { GetCursorInfo(&mut after) } == 0 || !permitted() {
            return None;
        }
        if !over(&after) {
            return self.history.observe(false, Shape::Default);
        }
        if after.hCursor != before.hCursor || after.flags != before.flags {
            return None;
        }
        self.history.observe(true, shape)
    }
}
