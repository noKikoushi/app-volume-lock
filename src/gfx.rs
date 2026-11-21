// Drawing helpers shared by the flyout, the tray icon and the app icon (Direct2D).
use windows::core::*;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

pub fn col(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

pub fn hex(v: u32, a: f32) -> D2D1_COLOR_F {
    col(((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0, a)
}

pub fn pt(x: f32, y: f32) -> D2D_POINT_2F {
    D2D_POINT_2F { x, y }
}

pub fn rect(l: f32, t: f32, r: f32, b: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: l, top: t, right: r, bottom: b }
}

pub fn rrect(l: f32, t: f32, r: f32, b: f32, rad: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT { rect: rect(l, t, r, b), radiusX: rad, radiusY: rad }
}

pub fn ellipse(x: f32, y: f32, r: f32) -> D2D1_ELLIPSE {
    D2D1_ELLIPSE { point: pt(x, y), radiusX: r, radiusY: r }
}

// 2D affine matrices, row-vector convention like Direct2D (p' = p * M)
pub fn identity() -> Matrix3x2 {
    Matrix3x2 { M11: 1.0, M12: 0.0, M21: 0.0, M22: 1.0, M31: 0.0, M32: 0.0 }
}
pub fn translate(x: f32, y: f32) -> Matrix3x2 {
    Matrix3x2 { M31: x, M32: y, ..identity() }
}
pub fn scale(s: f32) -> Matrix3x2 {
    Matrix3x2 { M11: s, M22: s, ..identity() }
}
pub fn rotate(deg: f32) -> Matrix3x2 {
    let (s, c) = deg.to_radians().sin_cos();
    Matrix3x2 { M11: c, M12: s, M21: -s, M22: c, M31: 0.0, M32: 0.0 }
}
pub fn mul(a: Matrix3x2, b: Matrix3x2) -> Matrix3x2 {
    Matrix3x2 {
        M11: a.M11 * b.M11 + a.M12 * b.M21,
        M12: a.M11 * b.M12 + a.M12 * b.M22,
        M21: a.M21 * b.M11 + a.M22 * b.M21,
        M22: a.M21 * b.M12 + a.M22 * b.M22,
        M31: a.M31 * b.M11 + a.M32 * b.M21 + b.M31,
        M32: a.M31 * b.M12 + a.M32 * b.M22 + b.M32,
    }
}

pub fn round_stroke(f: &ID2D1Factory) -> Result<ID2D1StrokeStyle> {
    unsafe {
        f.CreateStrokeStyle(
            &D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            },
            None,
        )
    }
}

/// Runs `draw` with a transform that maps a 24x24 icon box onto (cx, cy, size).
unsafe fn in_box(rt: &ID2D1RenderTarget, cx: f32, cy: f32, size: f32, draw: impl FnOnce(Matrix3x2)) {
    let mut old = identity();
    rt.GetTransform(&mut old);
    let s = size / 24.0;
    let base = mul(mul(scale(s), translate(cx - 12.0 * s, cy - 12.0 * s)), old);
    draw(base);
    rt.SetTransform(&old);
}

/// Padlock. `closed` 0 = open (shackle lifted and tilted), 1 = closed.
pub unsafe fn draw_lock(rt: &ID2D1RenderTarget, f: &ID2D1Factory, st: &ID2D1StrokeStyle, b: &ID2D1Brush, cx: f32, cy: f32, size: f32, closed: f32) {
    in_box(rt, cx, cy, size, |base| {
        rt.SetTransform(&base);
        rt.FillRoundedRectangle(&rrect(5.0, 11.0, 19.0, 21.0, 2.5), b);
        let Ok(geo) = f.CreatePathGeometry() else { return };
        let Ok(sink) = geo.Open() else { return };
        sink.BeginFigure(pt(8.0, 11.0), D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddLine(pt(8.0, 7.5));
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: pt(16.0, 7.5),
            size: D2D_SIZE_F { width: 4.0, height: 4.0 },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_SMALL,
        });
        sink.AddLine(pt(16.0, 11.0));
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        if sink.Close().is_err() {
            return;
        }
        let k = 1.0 - closed.clamp(0.0, 1.0);
        // open: lifted by 2.5 and turned 14° around the right foot of the shackle
        let local = mul(mul(translate(-16.0, -11.0), rotate(14.0 * k)), translate(16.0 + 3.0 * k, 11.0 - 2.5 * k));
        rt.SetTransform(&mul(local, base));
        rt.DrawGeometry(&geo, b, 2.2, st);
    });
}

/// Ceiling: a bar with an arrow pointing up to it.
pub unsafe fn draw_ceiling(rt: &ID2D1RenderTarget, st: &ID2D1StrokeStyle, b: &ID2D1Brush, cx: f32, cy: f32, size: f32) {
    in_box(rt, cx, cy, size, |base| {
        rt.SetTransform(&base);
        rt.DrawLine(pt(5.0, 4.5), pt(19.0, 4.5), b, 2.2, st);
        rt.DrawLine(pt(12.0, 20.0), pt(12.0, 9.5), b, 2.2, st);
        rt.DrawLine(pt(7.5, 14.0), pt(12.0, 9.5), b, 2.2, st);
        rt.DrawLine(pt(16.5, 14.0), pt(12.0, 9.5), b, 2.2, st);
    });
}

/// Speaker with a cross: always muted.
pub unsafe fn draw_mute(rt: &ID2D1RenderTarget, f: &ID2D1Factory, st: &ID2D1StrokeStyle, b: &ID2D1Brush, cx: f32, cy: f32, size: f32) {
    in_box(rt, cx, cy, size, |base| {
        rt.SetTransform(&base);
        let Ok(geo) = f.CreatePathGeometry() else { return };
        let Ok(sink) = geo.Open() else { return };
        sink.BeginFigure(pt(2.5, 9.0), D2D1_FIGURE_BEGIN_FILLED);
        for p in [pt(6.5, 9.0), pt(11.5, 4.5), pt(11.5, 19.5), pt(6.5, 15.0), pt(2.5, 15.0)] {
            sink.AddLine(p);
        }
        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
        if sink.Close().is_ok() {
            rt.FillGeometry(&geo, b, None);
        }
        rt.DrawLine(pt(15.5, 9.0), pt(21.5, 15.0), b, 2.2, st);
        rt.DrawLine(pt(21.5, 9.0), pt(15.5, 15.0), b, 2.2, st);
    });
}

pub unsafe fn draw_x(rt: &ID2D1RenderTarget, st: &ID2D1StrokeStyle, b: &ID2D1Brush, cx: f32, cy: f32, size: f32) {
    in_box(rt, cx, cy, size, |base| {
        rt.SetTransform(&base);
        rt.DrawLine(pt(6.0, 6.0), pt(18.0, 18.0), b, 2.0, st);
        rt.DrawLine(pt(18.0, 6.0), pt(6.0, 18.0), b, 2.0, st);
    });
}

/// The app's mark on a 16x16 grid: a speaker with a small closed padlock.
pub unsafe fn draw_mark(rt: &ID2D1RenderTarget, f: &ID2D1Factory, st: &ID2D1StrokeStyle, b: &ID2D1Brush, x: f32, y: f32, size: f32) -> Result<()> {
    let mut old = identity();
    rt.GetTransform(&mut old);
    rt.SetTransform(&mul(mul(scale(size / 16.0), translate(x, y)), old));
    let geo = f.CreatePathGeometry()?;
    let sink = geo.Open()?;
    sink.BeginFigure(pt(1.0, 5.6), D2D1_FIGURE_BEGIN_FILLED);
    for p in [pt(3.6, 5.6), pt(7.6, 2.0), pt(7.6, 14.0), pt(3.6, 10.4), pt(1.0, 10.4)] {
        sink.AddLine(p);
    }
    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
    sink.Close()?;
    rt.FillGeometry(&geo, b, None);
    // padlock, bottom right
    rt.FillRoundedRectangle(&rrect(9.0, 9.6, 15.4, 15.0, 1.2), b);
    let sh = f.CreatePathGeometry()?;
    let s2 = sh.Open()?;
    s2.BeginFigure(pt(10.5, 9.8), D2D1_FIGURE_BEGIN_HOLLOW);
    s2.AddLine(pt(10.5, 8.1));
    s2.AddArc(&D2D1_ARC_SEGMENT {
        point: pt(13.9, 8.1),
        size: D2D_SIZE_F { width: 1.7, height: 1.7 },
        rotationAngle: 0.0,
        sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
        arcSize: D2D1_ARC_SIZE_SMALL,
    });
    s2.AddLine(pt(13.9, 9.8));
    s2.EndFigure(D2D1_FIGURE_END_OPEN);
    s2.Close()?;
    rt.DrawGeometry(&sh, b, 1.3, st);
    rt.SetTransform(&old);
    Ok(())
}

pub fn wic() -> Result<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

/// Draws into an offscreen premultiplied BGRA bitmap.
pub unsafe fn render(f: &ID2D1Factory, w: u32, h: u32, dpi: f32, draw: impl FnOnce(&ID2D1RenderTarget) -> Result<()>) -> Result<IWICBitmap> {
    let bmp = wic()?.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
    let props = D2D1_RENDER_TARGET_PROPERTIES {
        r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
        pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
        dpiX: dpi,
        dpiY: dpi,
        usage: D2D1_RENDER_TARGET_USAGE_NONE,
        minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
    };
    let rt = f.CreateWicBitmapRenderTarget(&bmp, &props)?;
    rt.BeginDraw();
    let r = draw(&rt);
    rt.EndDraw(None, None)?;
    r?;
    Ok(bmp)
}

/// HICON from a premultiplied BGRA bitmap.
pub unsafe fn hicon(bmp: &IWICBitmap) -> Result<HICON> {
    let (mut w, mut h) = (0u32, 0u32);
    bmp.GetSize(&mut w, &mut h)?;
    let mut px = vec![0u8; (w * h * 4) as usize];
    bmp.CopyPixels(std::ptr::null(), w * 4, &mut px)?;
    // icons want straight (not premultiplied) alpha
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let color = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0)?;
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
    let mask_bytes = vec![0u8; (((w + 15) / 16) * 2 * h) as usize];
    let mask = CreateBitmap(w as i32, h as i32, 1, 1, Some(mask_bytes.as_ptr() as _));
    let info = ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon
}

/// Tray icon: the mark in white (dark taskbar) or black (light taskbar).
pub fn tray_icon(px: u32, white: bool) -> Result<HICON> {
    unsafe {
        let f: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let st = round_stroke(&f)?;
        let bmp = render(&f, px, px, 96.0, |rt| {
            let c = if white { col(1.0, 1.0, 1.0, 1.0) } else { col(0.0, 0.0, 0.0, 1.0) };
            let b = rt.CreateSolidColorBrush(&c, None)?;
            draw_mark(rt, &f, &st, &b.cast()?, 0.0, 0.0, px as f32)
        })?;
        hicon(&bmp)
    }
}
