// Tray icon and the Windows 11-style flyout (Direct2D + DirectWrite on an acrylic backdrop).
// Rows look like the Volume Mixer: the app's icon, a long slider, then Max / Mute / Lock.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::audio::{self, Audible, SHARED, SYSTEM, WM_APP_STATUS};
use crate::gfx::{self, col, ellipse, hex, rect, rrect};
use crate::settings::{self, AppCfg, Mode, Settings};
use crate::startup;
use crate::theme::{self, Theme};

const WM_APP_TRAY: u32 = WM_APP + 2;
const WM_APP_SHOW: u32 = WM_APP + 3;
const WM_MOUSELEAVE: u32 = 0x02A3;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1; // NINF_KEY
const CLASS: PCWSTR = w!("AppVolumeLockFlyout");
const TRAY_ID: u32 = 1;
const TIMER_ANIM: usize = 1;
const TIMER_TIP: usize = 2;
/// tooltips only after the pointer rests a while, so passing over the buttons stays quiet
const TIP_DELAY: Duration = Duration::from_millis(1000);
const LOCK_ANIM: Duration = Duration::from_millis(220);
// Layout, in device-independent pixels. Row 0 is Windows' system sounds (unless hidden
// in Settings), then the apps.
const W: f32 = 376.0;
const HEAD: f32 = 48.0;
const ROW: f32 = 48.0;
static SHOW_SYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn show_sys() -> bool {
    SHOW_SYS.load(Relaxed)
}

/// top of the first app row
fn apps_top() -> f32 {
    if show_sys() { HEAD + ROW + 9.0 } else { HEAD }
}

// The flyout grows up to 30 % of the screen's height; past that, the list scrolls.
thread_local! {
    static SCROLL: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    static ADD_SCROLL: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) };
    static MAX_H: std::cell::Cell<f32> = const { std::cell::Cell::new(10000.0) };
}

fn scroll() -> f32 {
    SCROLL.with(|c| c.get())
}
fn add_scroll() -> f32 {
    ADD_SCROLL.with(|c| c.get())
}
fn max_h() -> f32 {
    MAX_H.with(|c| c.get())
}

/// height of the visible part of the apps list
fn list_view_h(n_apps: usize) -> f32 {
    let full = n_apps.max(1) as f32 * ROW;
    full.min((max_h() - apps_top() - 4.0 - FOOT - PAD_B).max(ROW * 2.0))
}

fn list_max_scroll(n_apps: usize) -> f32 {
    (n_apps.max(1) as f32 * ROW - list_view_h(n_apps)).max(0.0)
}

fn add_full_h(add: &[Kind]) -> f32 {
    add.iter().map(|&k| kind_h(k)).sum()
}

fn add_view_h(add: &[Kind]) -> f32 {
    add_full_h(add).min((max_h() - HEAD - PAD_B - 4.0).max(ADD_ROW * 3.0))
}

fn add_max_scroll(add: &[Kind]) -> f32 {
    (add_full_h(add) - add_view_h(add)).max(0.0)
}

fn clamp_scrolls(n_apps: usize, add: &[Kind]) {
    SCROLL.with(|c| c.set(c.get().clamp(0.0, list_max_scroll(n_apps))));
    ADD_SCROLL.with(|c| c.set(c.get().clamp(0.0, add_max_scroll(add))));
}

/// A thin scrollbar like Windows' (only when the list overflows).
unsafe fn scrollbar(rt: &ID2D1RenderTarget, b: &ID2D1SolidColorBrush, c: D2D1_COLOR_F, top: f32, view: f32, full: f32, pos: f32) {
    if full <= view + 0.5 {
        return;
    }
    let h = (view * view / full).max(24.0);
    let y = top + (view - h) * (pos / (full - view));
    b.SetColor(&c);
    rt.FillRoundedRectangle(&rrect(W - 6.0, y + 2.0, W - 3.0, y + h - 2.0, 1.5), b);
}

// Settings page rows
const SET_ROWS: usize = 6;
const SET_SEP: f32 = 9.0; // gap before About
const TOGGLE_W: f32 = 40.0;
const SEG_W: f32 = 98.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SetHit {
    Startup,
    /// true = all output devices, false = only the default one
    Devices(bool),
    /// pointer on a row's label: a short explanation of that setting
    Info(usize),
    System,
    UnlockAll,
    GitHub,
    Quit,
}

fn set_row_y(i: usize) -> f32 {
    HEAD + i as f32 * ROW + if i >= 4 { SET_SEP } else { 0.0 }
}
/// no footer row any more: "Add an app" is the small + in the header
const FOOT: f32 = 0.0;
const PAD_B: f32 = 6.0;
const ADD_ROW: f32 = 44.0;
const R: f32 = W - 12.0;
const RM: (f32, f32) = (R - 26.0, R);
const LOCK: (f32, f32) = (R - 60.0, R - 30.0);
const MUTE: (f32, f32) = (R - 92.0, R - 62.0);
const MAXC: (f32, f32) = (R - 124.0, R - 94.0);
const ICON: (f32, f32) = (14.0, 46.0);
const SLIDER: (f32, f32) = (62.0, R - 140.0);
const GEAR: (f32, f32) = (W - 44.0, W - 12.0);
const PLUS: (f32, f32) = (W - 78.0, W - 46.0);
const ICON_PX: f32 = 24.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    None,
    Gear,
    Back,
    Icon(usize),
    Slider(usize),
    Max(usize),
    Mute(usize),
    Lock(usize),
    Remove(usize),
    Row(usize),
    Yes(usize),
    No(usize),
    Add,
    AddItem(usize),
    Browse,
    Set(SetHit),
    MenuOpen,
    MenuQuit,
}

/// The rows of "Choose an app".
#[derive(Clone)]
enum Entry {
    Header(&'static str),
    App(Audible),
    Browse,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Header,
    App,
    Browse,
}

const ADD_HEADER: f32 = 30.0;

fn kind_h(k: Kind) -> f32 {
    if k == Kind::Header { ADD_HEADER } else { ADD_ROW }
}

impl Hit {
    fn row(self) -> Option<usize> {
        match self {
            Hit::Icon(i) | Hit::Slider(i) | Hit::Max(i) | Hit::Mute(i) | Hit::Lock(i) | Hit::Remove(i) | Hit::Row(i) | Hit::Yes(i) | Hit::No(i) => Some(i),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Add,
    Settings,
    /// the small right-click menu of the tray icon
    Menu,
}

const MENU_W: f32 = 210.0;
const MENU_ROW: f32 = 36.0;

fn view_w(view: View) -> f32 {
    if view == View::Menu { MENU_W } else { W }
}

struct RowView {
    name: String,
    volume: u32,
    mode: Mode,
    running: bool,
    closed: f32,
    icon: Option<ID2D1Bitmap>,
}

enum AddView {
    Header(&'static str),
    App { name: String, volume: Option<u32>, icon: Option<ID2D1Bitmap> },
    Browse,
}

struct Model {
    rows: Vec<RowView>,
    add_items: Vec<AddView>,
    view: View,
    hover: Hit,
    drag: Option<usize>,
    confirm: Option<usize>,
    tip: bool,
    /// Settings page: start with Windows, show system sounds, all devices
    prefs: (bool, bool, bool),
    /// just installed: say where to find it
    notice: bool,
}

fn row_y(i: usize) -> f32 {
    if i == 0 {
        HEAD
    } else {
        apps_top() + (i - 1) as f32 * ROW - scroll()
    }
}

fn foot_y(n_apps: usize) -> f32 {
    apps_top() + list_view_h(n_apps) + 4.0
}

fn height(n_apps: usize, view: View, add: &[Kind]) -> f32 {
    match view {
        View::Main => foot_y(n_apps) + FOOT + PAD_B,
        View::Add => HEAD + add_view_h(add) + PAD_B + 4.0,
        View::Settings => set_row_y(SET_ROWS) + PAD_B,
        View::Menu => MENU_ROW * 2.0 + 8.0,
    }
}

/// Right-hand control of each Settings row.
fn set_hit(i: usize, x: f32) -> Option<SetHit> {
    let r = W - 18.0;
    match i {
        0 if x >= r - TOGGLE_W - 8.0 => Some(SetHit::Startup),
        1 if x >= r - SEG_W * 2.0 => Some(SetHit::Devices(x < r - SEG_W)),
        2 if x >= r - TOGGLE_W - 8.0 => Some(SetHit::System),
        0..=2 if x < 200.0 => Some(SetHit::Info(i)),
        3 if x >= r - 110.0 => Some(SetHit::UnlockAll),
        4 if x >= r - 120.0 => Some(SetHit::GitHub),
        5 if x >= r - 110.0 => Some(SetHit::Quit),
        _ => None,
    }
}

fn hit(n_apps: usize, view: View, add: &[Kind], confirm: Option<usize>, x: f32, y: f32) -> Hit {
    if view == View::Menu {
        return if x < 4.0 || x > MENU_W - 4.0 || y < 4.0 {
            Hit::None
        } else if y < 4.0 + MENU_ROW {
            Hit::MenuOpen
        } else if y < 4.0 + MENU_ROW * 2.0 {
            Hit::MenuQuit
        } else {
            Hit::None
        };
    }
    if y < HEAD {
        if x >= GEAR.0 && x < GEAR.1 && y > 8.0 && y < 40.0 {
            return Hit::Gear;
        }
        if view == View::Main && x >= PLUS.0 && x < PLUS.1 && y > 8.0 && y < 40.0 {
            return Hit::Add;
        }
        if view != View::Main && x < 48.0 {
            return Hit::Back;
        }
        return Hit::None;
    }
    if x < 6.0 || x > W - 6.0 {
        return Hit::None;
    }
    if view == View::Settings {
        for i in 0..SET_ROWS {
            let t = set_row_y(i);
            if y >= t && y < t + ROW {
                return set_hit(i, x).map_or(Hit::None, Hit::Set);
            }
        }
        return Hit::None;
    }
    if view == View::Add {
        if y >= HEAD + add_view_h(add) {
            return Hit::None;
        }
        let mut top = HEAD - add_scroll();
        for (i, &k) in add.iter().enumerate() {
            let h = kind_h(k);
            if y >= top && y < top + h {
                return match k {
                    Kind::App => Hit::AddItem(i),
                    Kind::Browse => Hit::Browse,
                    Kind::Header => Hit::None,
                };
            }
            top += h;
        }
        return Hit::None;
    }
    let top = apps_top();
    let i = if show_sys() && y < HEAD + ROW {
        Some(0)
    } else if y >= top && y < top + list_view_h(n_apps) && y + scroll() < top + n_apps as f32 * ROW {
        Some(1 + ((y - top + scroll()) / ROW) as usize)
    } else {
        None
    };
    if let Some(i) = i {
        if confirm == Some(i) {
            return if x >= R - 150.0 && x < R - 76.0 {
                Hit::Yes(i)
            } else if x >= R - 72.0 {
                Hit::No(i)
            } else {
                Hit::Row(i)
            };
        }
        return if x < ICON.1 {
            Hit::Icon(i)
        } else if x >= SLIDER.0 - 10.0 && x <= SLIDER.1 + 10.0 {
            Hit::Slider(i)
        } else if x >= MAXC.0 && x < MAXC.1 {
            Hit::Max(i)
        } else if x >= MUTE.0 && x < MUTE.1 {
            Hit::Mute(i)
        } else if x >= LOCK.0 && x < LOCK.1 {
            Hit::Lock(i)
        } else if x >= RM.0 && i > 0 {
            Hit::Remove(i)
        } else {
            Hit::Row(i)
        };
    }
    Hit::None
}

// ---------------------------------------------------------------- drawing

pub struct Res {
    d2d: ID2D1Factory,
    dw: IDWriteFactory,
    text: IDWriteTextFormat,
    text_r: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_l: IDWriteTextFormat,
    small_n: IDWriteTextFormat,
    letter: IDWriteTextFormat,
    title: IDWriteTextFormat,
    glyph: IDWriteTextFormat,
    stroke: ID2D1StrokeStyle,
}

fn has_font(dw: &IDWriteFactory, family: PCWSTR) -> bool {
    unsafe {
        let mut coll = None;
        if dw.GetSystemFontCollection(&mut coll, false).is_err() {
            return false;
        }
        let Some(coll) = coll else { return false };
        let (mut i, mut found) = (0u32, BOOL(0));
        coll.FindFamilyName(family, &mut i, &mut found).is_ok() && found.as_bool()
    }
}

impl Res {
    fn new() -> Result<Res> {
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let ui = if has_font(&dw, w!("Segoe UI Variable Text")) { w!("Segoe UI Variable Text") } else { w!("Segoe UI") };
            let icons = if has_font(&dw, w!("Segoe Fluent Icons")) { w!("Segoe Fluent Icons") } else { w!("Segoe MDL2 Assets") };
            let fmt = |family: PCWSTR, weight: DWRITE_FONT_WEIGHT, size: f32, align: DWRITE_TEXT_ALIGNMENT| -> Result<IDWriteTextFormat> {
                let f = dw.CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("en-us"))?;
                f.SetTextAlignment(align)?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                let sign = dw.CreateEllipsisTrimmingSign(&f)?;
                f.SetTrimming(&DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 }, &sign)?;
                Ok(f)
            };
            Ok(Res {
                text: fmt(ui, DWRITE_FONT_WEIGHT_NORMAL, 14.0, DWRITE_TEXT_ALIGNMENT_LEADING)?,
                text_r: fmt(ui, DWRITE_FONT_WEIGHT_NORMAL, 14.0, DWRITE_TEXT_ALIGNMENT_TRAILING)?,
                small: fmt(ui, DWRITE_FONT_WEIGHT_NORMAL, 12.0, DWRITE_TEXT_ALIGNMENT_CENTER)?,
                small_l: fmt(ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 12.0, DWRITE_TEXT_ALIGNMENT_LEADING)?,
                small_n: fmt(ui, DWRITE_FONT_WEIGHT_NORMAL, 12.0, DWRITE_TEXT_ALIGNMENT_LEADING)?,
                letter: fmt(ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0, DWRITE_TEXT_ALIGNMENT_CENTER)?,
                title: fmt(ui, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0, DWRITE_TEXT_ALIGNMENT_LEADING)?,
                glyph: fmt(icons, DWRITE_FONT_WEIGHT_NORMAL, 16.0, DWRITE_TEXT_ALIGNMENT_CENTER)?,
                stroke: gfx::round_stroke(&d2d)?,
                d2d,
                dw,
            })
        }
    }

    fn text_width(&self, s: &str, f: &IDWriteTextFormat) -> f32 {
        unsafe {
            let Ok(layout) = self.dw.CreateTextLayout(&wide(s), f, 1000.0, 40.0) else { return 200.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            if layout.GetMetrics(&mut m).is_err() {
                return 200.0;
            }
            m.widthIncludingTrailingWhitespace
        }
    }
}

/// The program's icon as a bitmap for this render target (like the Volume Mixer shows).
unsafe fn load_icon(rt: &ID2D1RenderTarget, path: &str, px: u32) -> Option<ID2D1Bitmap> {
    if path.is_empty() {
        return None;
    }
    if path.starts_with("shell:") {
        // a Store app: ask the shell for its logo, as the Volume Mixer does
        let made = (|| -> Result<ID2D1Bitmap> {
            let f: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
            let hbm = f.GetImage(SIZE { cx: px as i32, cy: px as i32 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)?;
            let wic = gfx::wic()?;
            let bmp = wic.CreateBitmapFromHBITMAP(hbm, None, WICBitmapUsePremultipliedAlpha);
            let _ = DeleteObject(hbm);
            let conv = wic.CreateFormatConverter()?;
            conv.Initialize(&bmp?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeMedianCut)?;
            rt.CreateBitmapFromWicBitmap(&conv, None)
        })();
        return made.ok();
    }
    let mut large = HICON::default();
    SHDefExtractIconW(&HSTRING::from(path), 0, 0, Some(&mut large), None, px & 0xffff).ok().ok()?;
    if large.is_invalid() {
        return None;
    }
    let made = (|| -> Result<ID2D1Bitmap> {
        let wic = gfx::wic()?;
        let bmp = wic.CreateBitmapFromHICON(large)?;
        let conv = wic.CreateFormatConverter()?;
        conv.Initialize(&bmp, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeMedianCut)?;
        rt.CreateBitmapFromWicBitmap(&conv, None)
    })();
    let _ = DestroyIcon(large);
    made.ok()
}

struct Pal {
    text: D2D1_COLOR_F,
    muted: D2D1_COLOR_F,
    hover: D2D1_COLOR_F,
    track: D2D1_COLOR_F,
    thumb: D2D1_COLOR_F,
    thumb_edge: D2D1_COLOR_F,
    line: D2D1_COLOR_F,
    bg: D2D1_COLOR_F,
    tip: D2D1_COLOR_F,
    danger: D2D1_COLOR_F,
    accent: D2D1_COLOR_F,
    on_accent: D2D1_COLOR_F,
    chip: D2D1_COLOR_F,
}

fn pal(t: &Theme) -> Pal {
    let accent = col(t.accent[0], t.accent[1], t.accent[2], 1.0);
    if t.dark {
        Pal {
            text: col(1.0, 1.0, 1.0, 1.0),
            muted: col(1.0, 1.0, 1.0, 0.62),
            hover: col(1.0, 1.0, 1.0, 0.07),
            track: col(1.0, 1.0, 1.0, 0.45),
            thumb: hex(0x454545, 1.0),
            thumb_edge: col(1.0, 1.0, 1.0, 0.1),
            line: col(1.0, 1.0, 1.0, 0.09),
            bg: hex(0x2c2c2c, 1.0),
            tip: hex(0x2b2b2b, 1.0),
            danger: hex(0xff99a4, 1.0),
            accent,
            on_accent: col(0.0, 0.0, 0.0, 0.9),
            chip: col(1.0, 1.0, 1.0, 0.08),
        }
    } else {
        Pal {
            text: hex(0x1a1a1a, 1.0),
            muted: col(0.0, 0.0, 0.0, 0.6),
            hover: col(0.0, 0.0, 0.0, 0.05),
            track: col(0.0, 0.0, 0.0, 0.45),
            thumb: hex(0xffffff, 1.0),
            thumb_edge: col(0.0, 0.0, 0.0, 0.16),
            line: col(0.0, 0.0, 0.0, 0.09),
            bg: hex(0xf3f3f3, 1.0),
            tip: hex(0xf9f9f9, 1.0),
            danger: hex(0xc42b1c, 1.0),
            accent,
            on_accent: col(1.0, 1.0, 1.0, 1.0),
            chip: col(0.0, 0.0, 0.0, 0.04),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

unsafe fn text(rt: &ID2D1RenderTarget, f: &IDWriteTextFormat, s: &str, r: D2D_RECT_F, b: &ID2D1SolidColorBrush) {
    rt.DrawText(&wide(s), f, &r, b, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
}

/// The app's icon, or its first letter in a circle when there is none.
unsafe fn draw_app_icon(rt: &ID2D1RenderTarget, res: &Res, _p: &Pal, b: &ID2D1SolidColorBrush, icon: &Option<ID2D1Bitmap>, name: &str, cx: f32, cy: f32, opacity: f32) {
    let h = ICON_PX / 2.0;
    match icon {
        Some(bmp) => {
            // clipped to a rounded square like the Volume Mixer's, so square logos
            // (and any light corners around them) don't stick out
            let sz = bmp.GetSize();
            let props = D2D1_BITMAP_BRUSH_PROPERTIES {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            };
            match rt.CreateBitmapBrush(bmp, Some(&props), None) {
                Ok(brush) if sz.width > 0.0 => {
                    let k = ICON_PX / sz.width;
                    brush.SetTransform(&gfx::mul(gfx::scale(k), gfx::translate(cx - h, cy - h)));
                    brush.SetOpacity(opacity);
                    rt.FillRoundedRectangle(&rrect(cx - h, cy - h, cx + h, cy + h, 6.0), &brush);
                }
                _ => rt.DrawBitmap(bmp, Some(&rect(cx - h, cy - h, cx + h, cy + h)), opacity, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None),
            }
        }
        None => {
            // a rounded square in a colour picked from the name, with its first letter
            const COLORS: [u32; 6] = [0x0067C0, 0x7A3FC0, 0x0E8A6A, 0xC24E1C, 0xB8326E, 0x4A5A6A];
            let k = name.bytes().fold(0usize, |a, c| a.wrapping_mul(31).wrapping_add(c as usize));
            b.SetColor(&hex(COLORS[k % COLORS.len()], opacity));
            rt.FillRoundedRectangle(&rrect(cx - h, cy - h, cx + h, cy + h, 6.0), b);
            b.SetColor(&col(1.0, 1.0, 1.0, opacity));
            let l: String = name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
            text(rt, &res.letter, &l, rect(cx - h, cy - h, cx + h, cy + h), b);
        }
    }
}

/// A small floating label (tooltip / value bubble).
unsafe fn bubble(rt: &ID2D1RenderTarget, res: &Res, p: &Pal, b: &ID2D1SolidColorBrush, s: &str, cx: f32, row_top: f32) {
    // one line per "\n"
    let lines = s.lines().count().max(1) as f32;
    let bh = 10.0 + lines * 16.0;
    let tw = s.lines().map(|l| res.text_width(l, &res.small_n)).fold(0.0, f32::max) + 20.0;
    let x = (cx - tw / 2.0).clamp(8.0, W - 8.0 - tw);
    let ty = if row_top - bh < 4.0 { row_top + ROW + 2.0 } else { row_top - bh + 2.0 };
    b.SetColor(&p.tip);
    rt.FillRoundedRectangle(&rrect(x, ty, x + tw, ty + bh, 4.0), b);
    b.SetColor(&p.line);
    rt.DrawRoundedRectangle(&rrect(x + 0.5, ty + 0.5, x + tw - 0.5, ty + bh - 0.5, 4.0), b, 1.0, None);
    b.SetColor(&p.text);
    for (k, l) in s.lines().enumerate() {
        let ly = ty + 5.0 + k as f32 * 16.0;
        text(rt, &res.small_n, l, rect(x + 10.0, ly, x + tw, ly + 16.0), b);
    }
}

/// Draws the flyout content (not the background) at the current transform.
unsafe fn draw(rt: &ID2D1RenderTarget, res: &Res, m: &Model, th: &Theme) -> Result<()> {
    let p = pal(th);
    rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
    let b = rt.CreateSolidColorBrush(&p.text, None)?;
    let bb: ID2D1Brush = b.cast()?;
    let fill = |c: D2D1_COLOR_F, r: D2D1_ROUNDED_RECT| {
        b.SetColor(&c);
        rt.FillRoundedRectangle(&r, &b);
    };

    if m.view == View::Menu {
        for (k, (h, glyph, label)) in [(Hit::MenuOpen, "\u{E8A7}", "Open App Volume Lock"), (Hit::MenuQuit, "\u{E711}", "Quit")].into_iter().enumerate() {
            let y = 4.0 + k as f32 * MENU_ROW;
            if m.hover == h {
                fill(p.hover, rrect(4.0, y, MENU_W - 4.0, y + MENU_ROW, 4.0));
            }
            b.SetColor(&p.text);
            text(rt, &res.glyph, glyph, rect(8.0, y, 36.0, y + MENU_ROW), &b);
            text(rt, &res.text, label, rect(42.0, y, MENU_W - 10.0, y + MENU_ROW), &b);
        }
        return Ok(());
    }

    // header
    match m.view {
        View::Main | View::Menu => {
            b.SetColor(&p.text);
            text(rt, &res.title, "App Volume Lock", rect(18.0, 0.0, 260.0, HEAD), &b);
        }
        View::Add | View::Settings => {
            if m.hover == Hit::Back {
                fill(p.hover, rrect(8.0, 8.0, 40.0, 40.0, 4.0));
            }
            b.SetColor(&p.text);
            text(rt, &res.glyph, "\u{E72B}", rect(8.0, 8.0, 40.0, 40.0), &b);
            let t = if m.view == View::Add { "Choose an app" } else { "Settings" };
            text(rt, &res.title, t, rect(50.0, 0.0, 260.0, HEAD), &b);
        }
    }
    if m.view != View::Settings {
        if m.hover == Hit::Gear {
            fill(p.hover, rrect(GEAR.0, 8.0, GEAR.1, 40.0, 4.0));
        }
        b.SetColor(if m.hover == Hit::Gear { &p.text } else { &p.muted });
        text(rt, &res.glyph, "\u{E713}", rect(GEAR.0, 8.0, GEAR.1, 40.0), &b);
    }
    if m.view == View::Main {
        // "Add an app": a quiet + next to the gear
        if m.hover == Hit::Add {
            fill(p.hover, rrect(PLUS.0, 8.0, PLUS.1, 40.0, 4.0));
        }
        b.SetColor(if m.hover == Hit::Add { &p.text } else { &p.muted });
        text(rt, &res.glyph, "\u{E710}", rect(PLUS.0, 8.0, PLUS.1, 40.0), &b);
        if m.hover == Hit::Add && m.tip {
            bubble(rt, res, &p, &b, "Add an app", (PLUS.0 + PLUS.1) / 2.0, 0.0);
        }
    }

    if m.view == View::Settings {
        let (start, sys, all_devices) = m.prefs;
        let r = W - 18.0;
        // Windows 11 toggle switch
        let toggle = |on: bool, over: bool, cy: f32| {
            let (l, t) = (r - TOGGLE_W, cy - 10.0);
            if on {
                fill(p.accent, rrect(l, t, r, t + 20.0, 10.0));
                b.SetColor(&p.on_accent);
                rt.FillEllipse(&ellipse(r - 10.0, cy, if over { 7.0 } else { 6.0 }), &b);
            } else {
                if over {
                    fill(p.hover, rrect(l, t, r, t + 20.0, 10.0));
                }
                b.SetColor(&p.muted);
                rt.DrawRoundedRectangle(&rrect(l + 0.5, t + 0.5, r - 0.5, t + 19.5, 9.5), &b, 1.0, None);
                rt.FillEllipse(&ellipse(l + 10.0, cy, if over { 6.0 } else { 5.0 }), &b);
            }
        };
        let button = |label: &str, over: bool, cy: f32, color: D2D1_COLOR_F| {
            let tw = res.text_width(label, &res.small) + 28.0;
            let bx = rrect(r - tw, cy - 15.0, r, cy + 15.0, 4.0);
            fill(if over { p.hover } else { p.chip }, bx);
            b.SetColor(&p.line);
            rt.DrawRoundedRectangle(&rrect(r - tw + 0.5, cy - 14.5, r - 0.5, cy + 14.5, 4.0), &b, 1.0, None);
            b.SetColor(&color);
            text(rt, &res.small, label, rect(r - tw, cy - 15.0, r, cy + 15.0), &b);
        };
        let rows: [(&str, &str); SET_ROWS] = [
            ("Start with Windows", ""),
            ("Lock on", "Which outputs to lock"),
            ("Show System sounds", ""),
            ("Unlock everything", "Turns off every Lock, Max and Mute"),
            ("App Volume Lock", concat!("Version ", env!("CARGO_PKG_VERSION"), " · free and open source")),
            ("Quit", "Locks stop until it runs again"),
        ];
        for (i, (label, sub)) in rows.iter().enumerate() {
            let y = set_row_y(i);
            let cy = y + ROW / 2.0;
            b.SetColor(&p.text);
            if sub.is_empty() {
                text(rt, &res.text, label, rect(18.0, y, 220.0, y + ROW), &b);
            } else {
                text(rt, &res.text, label, rect(18.0, y + 4.0, 260.0, cy + 2.0), &b);
                b.SetColor(&p.muted);
                text(rt, &res.small_n, sub, rect(18.0, cy, 280.0, y + ROW - 6.0), &b);
            }
            let over = |h: SetHit| m.hover == Hit::Set(h);
            match i {
                0 => toggle(start, over(SetHit::Startup), cy),
                1 => {
                    let l = r - SEG_W * 2.0;
                    fill(p.chip, rrect(l, cy - 15.0, r, cy + 15.0, 5.0));
                    for (k, (all, name)) in [(true, "All devices"), (false, "Current device")].into_iter().enumerate() {
                        let sl = l + k as f32 * SEG_W;
                        let sel = all_devices == all;
                        if sel {
                            fill(p.accent, rrect(sl + 2.0, cy - 13.0, sl + SEG_W - 2.0, cy + 13.0, 4.0));
                        } else if over(SetHit::Devices(all)) {
                            fill(p.hover, rrect(sl + 2.0, cy - 13.0, sl + SEG_W - 2.0, cy + 13.0, 4.0));
                        }
                        b.SetColor(if sel { &p.on_accent } else { &p.text });
                        text(rt, &res.small, name, rect(sl, cy - 15.0, sl + SEG_W, cy + 15.0), &b);
                    }
                }
                2 => toggle(sys, over(SetHit::System), cy),
                3 => button("Unlock all", over(SetHit::UnlockAll), cy, p.text),
                4 => button("GitHub page ↗", over(SetHit::GitHub), cy, p.text),
                _ => button("Quit", over(SetHit::Quit), cy, p.danger),
            }
            if i == 3 {
                b.SetColor(&p.line);
                rt.FillRectangle(&rect(14.0, y + ROW + 4.0, W - 14.0, y + ROW + 5.0), &b);
            }
        }
        if let (Hit::Set(SetHit::Info(i)), true) = (m.hover, m.tip) {
            let t = match i {
                0 => "Opens quietly in the tray when you sign in",
                1 => "All devices: every speaker and headset\nCurrent device: only the one you're using",
                _ => "Windows' own sounds: alerts and notifications\nHidden: they're left as they are",
            };
            bubble(rt, res, &p, &b, t, 120.0, set_row_y(i));
        }
        return Ok(());
    }

    if m.view == View::Add {
        let kinds: Vec<Kind> = m
            .add_items
            .iter()
            .map(|a| match a {
                AddView::Header(_) => Kind::Header,
                AddView::App { .. } => Kind::App,
                AddView::Browse => Kind::Browse,
            })
            .collect();
        let view_h = add_view_h(&kinds);
        rt.PushAxisAlignedClip(&rect(0.0, HEAD, W, HEAD + view_h), D2D1_ANTIALIAS_MODE_ALIASED);
        let mut y = HEAD - add_scroll();
        for (i, a) in m.add_items.iter().enumerate() {
            match a {
                AddView::Header(t) => {
                    b.SetColor(&p.muted);
                    text(rt, &res.small_l, t, rect(18.0, y + 6.0, W - 18.0, y + ADD_HEADER), &b);
                    y += ADD_HEADER;
                }
                AddView::App { name, volume, icon } => {
                    let cy = y + ADD_ROW / 2.0;
                    if m.hover == Hit::AddItem(i) {
                        fill(p.hover, rrect(6.0, y + 2.0, W - 6.0, y + ADD_ROW - 2.0, 4.0));
                    }
                    draw_app_icon(rt, res, &p, &b, icon, name, (ICON.0 + ICON.1) / 2.0, cy, 1.0);
                    b.SetColor(&p.text);
                    text(rt, &res.text, name, rect(ICON.1 + 10.0, y, W - 70.0, y + ADD_ROW), &b);
                    if let Some(v) = volume {
                        b.SetColor(&p.muted);
                        text(rt, &res.text_r, &format!("{v}%"), rect(W - 70.0, y, W - 18.0, y + ADD_ROW), &b);
                    }
                    y += ADD_ROW;
                }
                AddView::Browse => {
                    let over = m.hover == Hit::Browse;
                    b.SetColor(&p.line);
                    rt.FillRectangle(&rect(14.0, y + 2.0, W - 14.0, y + 3.0), &b);
                    if over {
                        fill(p.hover, rrect(6.0, y + 5.0, W - 6.0, y + ADD_ROW - 1.0, 4.0));
                    }
                    b.SetColor(if over { &p.text } else { &p.muted });
                    text(rt, &res.glyph, "\u{E8E5}", rect(ICON.0, y + 3.0, ICON.1, y + ADD_ROW), &b);
                    text(rt, &res.text, "Browse for a program…", rect(ICON.1 + 10.0, y + 3.0, W - 18.0, y + ADD_ROW), &b);
                    y += ADD_ROW;
                }
            }
        }
        rt.PopAxisAlignedClip();
        scrollbar(rt, &b, p.muted, HEAD, view_h, add_full_h(&kinds), add_scroll());
        return Ok(());
    }

    let mut later: Option<(String, f32, f32)> = None; // bubble drawn on top of everything
    let n_apps = m.rows.len().saturating_sub(1);
    let list_h = list_view_h(n_apps);
    let mut clipped = false;
    for (i, row) in m.rows.iter().enumerate() {
        if i == 0 && !show_sys() {
            continue;
        }
        if i == 1 {
            // the apps list scrolls inside its own area
            rt.PushAxisAlignedClip(&rect(0.0, apps_top(), W, apps_top() + list_h), D2D1_ANTIALIAS_MODE_ALIASED);
            clipped = true;
        }
        let y = row_y(i);
        if i > 0 && (y + ROW < apps_top() || y > apps_top() + list_h) {
            continue; // scrolled out of sight
        }
        let cy = y + ROW / 2.0;
        let hovered = m.hover.row() == Some(i) || m.drag == Some(i);
        if hovered || m.confirm == Some(i) {
            fill(p.hover, rrect(6.0, y + 2.0, W - 6.0, y + ROW - 2.0, 4.0));
        }
        let icx = (ICON.0 + ICON.1) / 2.0;
        if i == 0 {
            b.SetColor(&p.text);
            text(rt, &res.glyph, "\u{E7F6}", rect(ICON.0, y, ICON.1, y + ROW), &b); // headphones, like the Volume Mixer
        } else {
            draw_app_icon(rt, res, &p, &b, &row.icon, &row.name, icx, cy, if row.running { 1.0 } else { 0.45 });
        }
        if m.confirm == Some(i) {
            b.SetColor(&p.text);
            text(rt, &res.text, &format!("Remove {}?", row.name), rect(ICON.1 + 8.0, y, R - 154.0, y + ROW), &b);
            for (h, l, r, label, c) in [(Hit::Yes(i), R - 150.0, R - 76.0, "Remove", p.danger), (Hit::No(i), R - 72.0, R, "Cancel", p.text)] {
                fill(if m.hover == h { p.hover } else { p.chip }, rrect(l, cy - 15.0, r, cy + 15.0, 4.0));
                b.SetColor(&p.line);
                rt.DrawRoundedRectangle(&rrect(l + 0.5, cy - 14.5, r - 0.5, cy + 14.5, 4.0), &b, 1.0, None);
                b.SetColor(&c);
                text(rt, &res.small, label, rect(l, cy - 15.0, r, cy + 15.0), &b);
            }
            continue;
        }

        // slider. Locked: frozen and greyed, it can't be moved until unlocked.
        let locked = row.mode == Mode::Lock;
        let frac = row.volume.min(100) as f32 / 100.0;
        let tx = SLIDER.0 + frac * (SLIDER.1 - SLIDER.0);
        let mut fc = if locked { p.muted } else { p.accent };
        let mut tc = p.track;
        if row.mode == Mode::Mute {
            fc.a *= 0.45;
            tc.a *= 0.45;
        }
        if locked {
            tc.a *= 0.5;
        }
        fill(tc, rrect(SLIDER.0, cy - 2.0, SLIDER.1, cy + 2.0, 2.0));
        fill(fc, rrect(SLIDER.0, cy - 2.0, tx.max(SLIDER.0 + 4.0), cy + 2.0, 2.0));
        b.SetColor(&p.thumb_edge);
        rt.FillEllipse(&ellipse(tx, cy, 10.0), &b);
        b.SetColor(&p.thumb);
        rt.FillEllipse(&ellipse(tx, cy, 9.0), &b);
        b.SetColor(&fc);
        let r_in = if locked { 4.0 } else if m.drag == Some(i) { 4.5 } else if m.hover == Hit::Slider(i) { 6.0 } else { 5.0 };
        rt.FillEllipse(&ellipse(tx, cy, r_in), &b);

        // Max / Mute / Lock: the one that's on is filled, like Windows' quick settings toggles
        for (h, (l, r), on) in [
            (Hit::Max(i), MAXC, row.mode == Mode::Max),
            (Hit::Mute(i), MUTE, row.mode == Mode::Mute),
            (Hit::Lock(i), LOCK, locked),
        ] {
            let over = m.hover == h;
            let bx = rrect(l, cy - 15.0, r, cy + 15.0, 5.0);
            if on {
                fill(p.accent, bx);
            } else if over {
                fill(p.hover, bx);
            }
            b.SetColor(if on { &p.on_accent } else if over { &p.text } else { &p.muted });
            let cx = (l + r) / 2.0;
            match h {
                Hit::Max(_) => gfx::draw_ceiling(rt, &res.stroke, &bb, cx, cy, 17.0),
                Hit::Mute(_) => gfx::draw_mute(rt, &res.d2d, &res.stroke, &bb, cx, cy, 17.0),
                _ => gfx::draw_lock(rt, &res.d2d, &res.stroke, &bb, cx, cy, 17.0, row.closed),
            }
        }
        if i > 0 {
            let over = m.hover == Hit::Remove(i);
            if over {
                fill(p.hover, rrect(RM.0 + 1.0, cy - 15.0, RM.1, cy + 15.0, 5.0));
            }
            b.SetColor(if over { &p.text } else { &p.muted });
            gfx::draw_x(rt, &res.stroke, &bb, (RM.0 + RM.1) / 2.0, cy, 10.0);
        }
        if i == 0 {
            b.SetColor(&p.line);
            rt.FillRectangle(&rect(14.0, HEAD + ROW + 4.0, W - 14.0, HEAD + ROW + 5.0), &b);
        }

        // value bubble over the thumb (like Windows' sliders), and tooltips
        let tip = match m.hover {
            Hit::Slider(j) if j == i && locked && m.tip => Some(format!("Locked at {}%. Unlock to change it.", row.volume)),
            Hit::Slider(j) if j == i && m.tip => Some(format!("{}%", row.volume)),
            Hit::Icon(j) if j == i && m.tip => Some(row.name.clone()),
            Hit::Max(j) if j == i && m.tip => Some(format!("Max {}%: it can go lower, never higher", row.volume)),
            Hit::Mute(j) if j == i && m.tip => Some("Mute: always keep it muted".into()),
            Hit::Lock(j) if j == i && m.tip => Some(if locked { "Unlock".into() } else { format!("Lock at {}%", row.volume) }),
            Hit::Remove(j) if j == i && m.tip => Some("Remove from the list".into()),
            _ => None,
        };
        let tip = if m.drag == Some(i) { Some(format!("{}%", row.volume)) } else { tip };
        if let Some(t) = tip {
            let cx = match m.hover {
                Hit::Icon(_) => (ICON.0 + ICON.1) / 2.0,
                Hit::Max(_) => (MAXC.0 + MAXC.1) / 2.0,
                Hit::Mute(_) => (MUTE.0 + MUTE.1) / 2.0,
                Hit::Lock(_) => (LOCK.0 + LOCK.1) / 2.0,
                Hit::Remove(_) => (RM.0 + RM.1) / 2.0,
                _ => tx,
            };
            later = Some((t, cx, y));
        }
    }
    if clipped {
        rt.PopAxisAlignedClip();
        scrollbar(rt, &b, p.muted, apps_top(), list_h, n_apps as f32 * ROW, scroll());
    }
    if n_apps == 0 {
        b.SetColor(&p.muted);
        text(rt, &res.text, "Click + to add an app.", rect(18.0, apps_top(), W - 18.0, apps_top() + ROW), &b);
    }

    if m.notice && later.is_none() {
        bubble(rt, res, &p, &b, "Installed. You'll find it in the Start menu.", W / 2.0, 0.0);
    }
    if let Some((t, cx, y)) = later {
        bubble(rt, res, &p, &b, &t, cx, y);
    }
    Ok(())
}

// ---------------------------------------------------------------- the window

struct Ui {
    hwnd: HWND,
    res: Res,
    rt: Option<ID2D1HwndRenderTarget>,
    icons: HashMap<String, Option<ID2D1Bitmap>>,
    settings: Settings,
    theme: Theme,
    backdrop: bool,
    view: View,
    hover: Hit,
    hover_at: Instant,
    drag: Option<usize>,
    confirm: Option<usize>,
    tracking: bool,
    anims: Vec<(String, Instant, bool)>,
    add_items: Vec<Entry>,
    open_apps: Vec<Audible>,
    visible: bool,
    closed_at: Instant,
    menu_open: bool,
    /// just installed: "Installed …" shows until the panel is first closed
    notice: bool,
    /// the last thing closed was the right-click menu
    closed_menu: bool,
    icon: HICON,
    taskbar_created: u32,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// Things that make Windows send us messages right away: done after the state is released.
enum Act {
    None,
    Toggle,
    Show,
    Hide,
    Resize,
    ShowMenu,
    Quit,
    Capture(bool),
    Browse,
}

fn lo(v: isize) -> i32 {
    (v & 0xffff) as i16 as i32
}
fn hi(v: isize) -> i32 {
    ((v >> 16) & 0xffff) as i16 as i32
}

/// Acrylic behind the flyout, like Windows' own flyouts (a tinted blur of what's behind).
unsafe fn set_acrylic(hwnd: HWND, dark: bool) -> bool {
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        color: u32,
        anim: u32,
    }
    #[repr(C)]
    struct Data {
        attr: u32,
        data: *mut core::ffi::c_void,
        size: usize,
    }
    let Ok(user) = GetModuleHandleW(w!("user32.dll")) else { return false };
    let Some(f) = GetProcAddress(user, s!("SetWindowCompositionAttribute")) else { return false };
    let swca: extern "system" fn(HWND, *mut Data) -> BOOL = std::mem::transmute(f);
    // AABBGGRR
    let color = if dark { 0x99_20_20_20 } else { 0x99_F3_F3_F3 };
    let mut policy = AccentPolicy { state: 4, flags: 0, color, anim: 0 }; // 4 = acrylic blur behind
    let mut data = Data { attr: 19, data: &mut policy as *mut _ as _, size: std::mem::size_of::<AccentPolicy>() }; // 19 = accent policy
    swca(hwnd, &mut data).as_bool()
}

impl Ui {
    fn scale(&self) -> f32 {
        unsafe { GetDpiForWindow(self.hwnd) as f32 / 96.0 }
    }

    fn n_rows(&self) -> usize {
        1 + self.settings.apps.len()
    }

    fn key(&self, i: usize) -> String {
        if i == 0 {
            SYSTEM.to_string()
        } else {
            self.settings.apps[i - 1].exe.clone()
        }
    }

    fn cfg(&self, i: usize) -> (u32, Mode) {
        if i == 0 {
            (self.settings.system_sounds.volume, self.settings.system_sounds.mode)
        } else {
            let a = &self.settings.apps[i - 1];
            (a.volume, a.mode)
        }
    }

    fn cfg_mut(&mut self, i: usize) -> (&mut u32, &mut Mode) {
        if i == 0 {
            let m = &mut self.settings.system_sounds;
            (&mut m.volume, &mut m.mode)
        } else {
            let a = &mut self.settings.apps[i - 1];
            (&mut a.volume, &mut a.mode)
        }
    }

    /// current volume if the app is playing (system sounds: always)
    fn current(&self, i: usize) -> Option<u32> {
        let s = SHARED.lock().unwrap();
        if i == 0 {
            s.system_now
        } else {
            match s.status.get(&self.settings.apps[i - 1].exe) {
                Some(&(true, v)) => Some(v),
                _ => None,
            }
        }
    }

    fn shown_volume(&self, i: usize) -> u32 {
        let (vol, mode) = self.cfg(i);
        match mode {
            Mode::Lock | Mode::Max => vol,
            _ if self.drag == Some(i) => vol,
            _ => self.current(i).unwrap_or(vol),
        }
    }

    fn icon_for(&mut self, path: &str) -> Option<ID2D1Bitmap> {
        if path.is_empty() {
            return None;
        }
        if let Some(i) = self.icons.get(path) {
            return i.clone();
        }
        let rt: ID2D1RenderTarget = self.rt.as_ref()?.cast().ok()?;
        let px = (ICON_PX * self.scale()).round() as u32;
        let bmp = unsafe { load_icon(&rt, path, px) };
        self.icons.insert(path.to_string(), bmp.clone());
        bmp
    }

    fn model(&mut self) -> Model {
        let mut rows = vec![];
        for i in 0..self.n_rows() {
            let (_, mode) = self.cfg(i);
            let key = self.key(i);
            let closed = match self.anims.iter().find(|x| x.0 == key) {
                Some((_, t0, to_closed)) => {
                    let k = (t0.elapsed().as_secs_f32() / LOCK_ANIM.as_secs_f32()).min(1.0);
                    let e = 1.0 - (1.0 - k).powi(3);
                    if *to_closed { e } else { 1.0 - e }
                }
                None => (mode == Mode::Lock) as u8 as f32,
            };
            let (name, icon) = if i == 0 {
                ("System sounds".to_string(), None)
            } else {
                let a = self.settings.apps[i - 1].clone();
                (a.name, self.icon_for(&a.path))
            };
            rows.push(RowView { name, volume: self.shown_volume(i), mode, running: self.current(i).is_some(), closed, icon });
        }
        let add_items = self
            .add_items
            .clone()
            .into_iter()
            .map(|e| match e {
                Entry::Header(t) => AddView::Header(t),
                Entry::Browse => AddView::Browse,
                Entry::App(a) => AddView::App { icon: self.icon_for(&a.path), name: a.name, volume: (a.volume <= 100).then_some(a.volume) },
            })
            .collect();
        Model {
            rows,
            add_items,
            view: self.view,
            hover: self.hover,
            drag: self.drag,
            confirm: self.confirm,
            tip: self.drag.is_none() && self.hover_at.elapsed() >= TIP_DELAY,
            prefs: (self.settings.start_with_windows && startup::is_set(), self.settings.show_system_sounds, self.settings.all_devices),
            notice: self.notice && self.view == View::Main,
        }
    }

    fn kinds(&self) -> Vec<Kind> {
        self.add_items
            .iter()
            .map(|e| match e {
                Entry::Header(_) => Kind::Header,
                Entry::App(_) => Kind::App,
                Entry::Browse => Kind::Browse,
            })
            .collect()
    }

    fn hit_at(&self, x: f32, y: f32) -> Hit {
        hit(self.settings.apps.len(), self.view, &self.kinds(), self.confirm, x, y)
    }

    fn add_app(&mut self, a: Audible) {
        let volume = if a.volume <= 100 { a.volume } else { 100 };
        self.settings.apps.push(AppCfg { exe: a.exe, name: a.name, volume, mode: Mode::Off, path: a.path });
        self.sync();
    }

    fn push(&self) {
        let mut s = SHARED.lock().unwrap();
        s.apps = self.settings.apps.clone();
        s.system = self.settings.system_sounds;
        s.all_devices = self.settings.all_devices;
    }

    fn sync(&self) {
        self.push();
        audio::wake();
        settings::save(&self.settings);
    }

    fn set_volume(&mut self, i: usize, v: u32) {
        let v = v.min(100);
        let key = self.key(i);
        let (vol, mode) = self.cfg_mut(i);
        if *mode == Mode::Lock {
            return; // locked: unlock first
        }
        *vol = v;
        let unlocked = matches!(*mode, Mode::Off | Mode::Mute);
        self.push();
        if unlocked {
            SHARED.lock().unwrap().once.insert(key);
        }
        audio::wake();
    }

    fn slider_value(x: f32) -> u32 {
        (((x - SLIDER.0) / (SLIDER.1 - SLIDER.0)).clamp(0.0, 1.0) * 100.0).round() as u32
    }

    /// Clicking Max / Mute / Lock: turns that mode on, or off if it was on.
    fn toggle_mode(&mut self, i: usize, m: Mode) {
        let shown = self.shown_volume(i);
        let key = self.key(i);
        let (vol, mode) = self.cfg_mut(i);
        let was = *mode;
        let new = if was == m { Mode::Off } else { m };
        if matches!(new, Mode::Lock | Mode::Max) {
            *vol = shown; // lock at what the slider shows
        }
        *mode = new;
        if was == Mode::Mute && new != Mode::Mute {
            SHARED.lock().unwrap().unmute.insert(key.clone());
        }
        if (was == Mode::Lock) != (new == Mode::Lock) {
            self.anims.retain(|x| x.0 != key);
            self.anims.push((key, Instant::now(), new == Mode::Lock));
            unsafe {
                SetTimer(self.hwnd, TIMER_ANIM, 15, None);
            }
        }
        self.sync();
    }

    /// "Playing sound" (what the Volume Mixer lists), then "Open apps" that haven't played
    /// sound yet (Windows doesn't list those until they do), then "Browse…".
    fn refresh_add_items(&mut self) {
        let listed = |exe: &str| self.settings.apps.iter().any(|x| exe.starts_with(&x.exe));
        let playing: Vec<Audible> = SHARED.lock().unwrap().audible.iter().filter(|a| !listed(&a.exe)).cloned().collect();
        let open: Vec<Audible> = self.open_apps.iter().filter(|a| !listed(&a.exe) && !playing.iter().any(|p| a.exe.starts_with(&p.exe) || p.exe.starts_with(&a.exe) || p.name.eq_ignore_ascii_case(&a.name))).cloned().collect();
        let mut items = vec![];
        if !playing.is_empty() {
            items.push(Entry::Header("Playing sound"));
            items.extend(playing.into_iter().map(Entry::App));
        }
        if !open.is_empty() {
            items.push(Entry::Header("Played sound before"));
            items.extend(open.into_iter().take(10).map(Entry::App));
        }
        items.push(Entry::Browse);
        self.add_items = items;
    }

    /// Remember each app's program file (for its icon when it isn't running).
    fn learn_paths(&mut self) {
        let (paths, audible) = {
            let s = SHARED.lock().unwrap();
            (s.paths.clone(), s.audible.clone())
        };
        let mut changed = false;
        // when each program last played sound (written at most hourly per program)
        let now = audio::now_secs();
        for a in audible {
            let t = self.settings.recent.entry(a.exe).or_insert(0);
            if *t + 3600 < now {
                *t = now;
                changed = true;
            }
        }
        if changed {
            self.settings.recent.retain(|_, t| *t + 30 * 86400 >= now);
        }
        for a in &mut self.settings.apps {
            if let Some(p) = paths.get(&a.exe) {
                if *p != a.path {
                    a.path = p.clone();
                    changed = true;
                }
            }
        }
        if changed {
            settings::save(&self.settings);
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, false);
        }
    }

    fn set_hover(&mut self, h: Hit) {
        if h != self.hover {
            let same_row = h.row().is_some() && h.row() == self.hover.row() && matches!(h, Hit::Slider(_)) == matches!(self.hover, Hit::Slider(_));
            self.hover = h;
            if !same_row {
                self.hover_at = Instant::now();
            }
            unsafe {
                SetTimer(self.hwnd, TIMER_TIP, TIP_DELAY.as_millis() as u32 + 10, None);
            }
            self.invalidate();
        }
    }

    /// A click on the Settings page.
    fn setting(&mut self, h: SetHit) -> Act {
        match h {
            SetHit::Startup => {
                let on = !(self.settings.start_with_windows && startup::is_set());
                self.settings.start_with_windows = on;
                startup::set(on);
                settings::save(&self.settings);
            }
            SetHit::Devices(all) => {
                self.settings.all_devices = all;
                self.sync();
            }
            SetHit::System => {
                let show = !self.settings.show_system_sounds;
                self.settings.show_system_sounds = show;
                SHOW_SYS.store(show, Relaxed);
                if !show {
                    // hidden means left alone: no invisible lock on Windows' sounds
                    if self.settings.system_sounds.mode == Mode::Mute {
                        SHARED.lock().unwrap().unmute.insert(SYSTEM.to_string());
                    }
                    self.settings.system_sounds.mode = Mode::Off;
                }
                self.sync();
            }
            SetHit::UnlockAll => {
                for i in 0..self.n_rows() {
                    let mode = self.cfg(i).1;
                    if mode != Mode::Off {
                        self.toggle_mode(i, mode); // the same as clicking the button that's on
                    }
                }
            }
            SetHit::GitHub => unsafe {
                ShellExecuteW(None, w!("open"), w!("https://github.com/noKikoushi/app-volume-lock"), None, None, SW_SHOWNORMAL);
                return Act::Hide;
            },
            SetHit::Quit => return Act::Quit,
            SetHit::Info(_) => {}
        }
        Act::None
    }

    /// Where the flyout goes: bottom right, above the taskbar (also when it hides itself).
    fn placement(&self) -> (i32, i32, i32, i32) {
        unsafe {
            let id = NOTIFYICONIDENTIFIER {
                cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
                hWnd: self.hwnd,
                uID: TRAY_ID,
                ..Default::default()
            };
            let anchor = match Shell_NotifyIconGetRect(&id) {
                Ok(r) => POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 },
                Err(_) => {
                    let mut p = POINT::default();
                    let _ = GetCursorPos(&mut p);
                    p
                }
            };
            let mon = MonitorFromPoint(anchor, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
            let _ = GetMonitorInfoW(mon, &mut mi);
            let (mut dx, mut dy) = (96u32, 96u32);
            let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
            let s = dx as f32 / 96.0;
            let w = (view_w(self.view) * s).round() as i32;
            // at most 30 % of the screen's height; the list scrolls past that
            let screen_h = (mi.rcMonitor.bottom - mi.rcMonitor.top) as f32 / s;
            MAX_H.with(|c| c.set((screen_h * 0.30).max(300.0)));
            clamp_scrolls(self.settings.apps.len(), &self.kinds());
            let h = (height(self.settings.apps.len(), self.view, &self.kinds()) * s).round() as i32;
            let gap = (12.0 * s).round() as i32;
            let mut work = mi.rcWork;
            // an auto-hiding taskbar doesn't reserve space, but Windows' flyouts still open above it
            let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
            let state = SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32;
            if state & ABS_AUTOHIDE != 0 && SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) != 0 {
                let tb = abd.rc;
                let mr = mi.rcMonitor;
                let on_this = tb.right > mr.left && tb.left < mr.right && tb.bottom > mr.top && tb.top < mr.bottom;
                if on_this {
                    match abd.uEdge {
                        ABE_BOTTOM => work.bottom = work.bottom.min(mr.bottom - (tb.bottom - tb.top)),
                        ABE_TOP => work.top = work.top.max(mr.top + (tb.bottom - tb.top)),
                        ABE_RIGHT => work.right = work.right.min(mr.right - (tb.right - tb.left)),
                        ABE_LEFT => work.left = work.left.max(mr.left + (tb.right - tb.left)),
                        _ => {}
                    }
                }
            }
            if self.view == View::Menu {
                // the right-click menu opens at the pointer, above it like Windows' tray menus
                let mut c = POINT::default();
                let _ = GetCursorPos(&mut c);
                let x = (c.x - w).clamp(work.left, work.right - w);
                let y = (c.y - h).clamp(work.top, work.bottom - h);
                return (x, y, w, h);
            }
            (work.right - w - gap, work.bottom - h - gap, w, h)
        }
    }

    fn apply_theme(&mut self) {
        self.theme = theme::read();
        unsafe {
            let dark = BOOL(self.theme.dark as i32);
            let _ = DwmSetWindowAttribute(self.hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as _, 4);
            self.backdrop = set_acrylic(self.hwnd, self.theme.dark);
            let px = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.hwnd)) as u32;
            if let Ok(icon) = gfx::tray_icon(px.max(16), self.theme.taskbar_dark) {
                let old = std::mem::replace(&mut self.icon, icon);
                self.tray(NIM_MODIFY);
                if !old.is_invalid() {
                    let _ = DestroyIcon(old);
                }
            }
        }
        self.invalidate();
    }

    fn tray(&self, msg: NOTIFY_ICON_MESSAGE) {
        unsafe {
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: self.hwnd,
                uID: TRAY_ID,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
                uCallbackMessage: WM_APP_TRAY,
                hIcon: self.icon,
                ..Default::default()
            };
            for (d, s) in nid.szTip.iter_mut().zip("App Volume Lock".encode_utf16()) {
                *d = s;
            }
            let _ = Shell_NotifyIconW(msg, &nid);
            if msg == NIM_ADD {
                nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
                let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
            }
        }
    }

    fn paint(&mut self) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            BeginPaint(self.hwnd, &mut ps);
            let _ = self.paint_inner();
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    unsafe fn paint_inner(&mut self) -> Result<()> {
        let mut rc = RECT::default();
        GetClientRect(self.hwnd, &mut rc)?;
        let size = D2D_SIZE_U { width: (rc.right - rc.left).max(1) as u32, height: (rc.bottom - rc.top).max(1) as u32 };
        if self.rt.is_none() {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                // drawn by the CPU: a panel this small is instant, and it keeps the graphics
                // driver (tens of MB and many threads) out of a tray app that's always running
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
                dpiX: 0.0,
                dpiY: 0.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES { hwnd: self.hwnd, pixelSize: size, presentOptions: D2D1_PRESENT_OPTIONS_NONE };
            self.rt = Some(self.res.d2d.CreateHwndRenderTarget(&props, &hp)?);
            self.icons.clear(); // bitmaps belong to a render target
        }
        let rt = self.rt.clone().unwrap();
        let cur = rt.GetPixelSize();
        if cur.width != size.width || cur.height != size.height {
            rt.Resize(&size)?;
        }
        let dpi = GetDpiForWindow(self.hwnd) as f32;
        let (ox, _) = {
            let (mut x, mut y) = (0.0, 0.0);
            rt.GetDpi(&mut x, &mut y);
            (x, y)
        };
        if ox != dpi {
            rt.SetDpi(dpi, dpi);
            self.icons.clear(); // icon size depends on the DPI
        }
        let m = self.model();
        rt.BeginDraw();
        rt.SetTransform(&gfx::identity());
        if self.backdrop {
            rt.Clear(Some(&col(0.0, 0.0, 0.0, 0.0)));
        } else {
            rt.Clear(Some(&pal(&self.theme).bg));
        }
        let _ = draw(&rt, &self.res, &m, &self.theme);
        if rt.EndDraw(None, None).is_err() {
            self.rt = None; // device lost: make a new one next time
        }
        Ok(())
    }

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> (Option<LRESULT>, Act) {
        let done = Some(LRESULT(0));
        match msg {
            WM_PAINT => {
                self.paint();
                (done, Act::None)
            }
            WM_ERASEBKGND => (Some(LRESULT(1)), Act::None),
            WM_MOUSEMOVE => {
                let s = self.scale();
                let (x, y) = (lo(lp.0) as f32 / s, hi(lp.0) as f32 / s);
                if !self.tracking {
                    self.tracking = true;
                    let mut tme = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: self.hwnd, dwHoverTime: 0 };
                    unsafe {
                        let _ = TrackMouseEvent(&mut tme);
                    }
                }
                if let Some(i) = self.drag {
                    self.set_volume(i, Self::slider_value(x));
                    self.invalidate();
                } else {
                    self.set_hover(self.hit_at(x, y));
                }
                (done, Act::None)
            }
            WM_MOUSELEAVE => {
                self.tracking = false;
                if self.drag.is_none() {
                    self.set_hover(Hit::None);
                }
                (done, Act::None)
            }
            WM_LBUTTONDOWN => {
                let s = self.scale();
                let (x, y) = (lo(lp.0) as f32 / s, hi(lp.0) as f32 / s);
                let h = self.hit_at(x, y);
                let mut act = Act::None;
                if !matches!(h, Hit::Yes(_) | Hit::No(_)) && self.confirm.take().is_some() {
                    self.invalidate();
                    return (done, Act::None); // a click elsewhere cancels "Remove …?"
                }
                self.hover_at = Instant::now() + Duration::from_secs(3600); // no tooltip right after a click
                match h {
                    Hit::Slider(i) => {
                        if self.cfg(i).1 != Mode::Lock {
                            self.drag = Some(i);
                            self.set_volume(i, Self::slider_value(x));
                            act = Act::Capture(true);
                        } else {
                            self.hover_at = Instant::now() - TIP_DELAY; // show "Locked at …"
                        }
                    }
                    Hit::Max(i) => self.toggle_mode(i, Mode::Max),
                    Hit::Mute(i) => self.toggle_mode(i, Mode::Mute),
                    Hit::Lock(i) => self.toggle_mode(i, Mode::Lock),
                    Hit::Remove(i) => self.confirm = Some(i),
                    Hit::Yes(i) => {
                        if i > 0 {
                            let a = self.settings.apps.remove(i - 1);
                            if a.mode == Mode::Mute {
                                SHARED.lock().unwrap().unmute.insert(a.exe);
                            }
                            self.sync();
                        }
                        self.confirm = None;
                        self.hover = Hit::None;
                        act = Act::Resize;
                    }
                    Hit::No(_) => self.confirm = None,
                    Hit::Add => {
                        self.view = View::Add;
                        self.hover = Hit::None;
                        // open apps that played sound before: what Windows remembers (it keeps no
                        // "last played" date, and only open apps are listed anyway), plus what we
                        // saw playing in the last 3 days
                        let since = 3 * 86400;
                        let mut known = audio::sound_history();
                        let now = audio::now_secs();
                        known.extend(self.settings.recent.iter().filter(|(_, &t)| t + since >= now).map(|(k, _)| k.clone()));
                        self.open_apps = audio::open_apps(&known);
                        self.refresh_add_items();
                        act = Act::Resize;
                    }
                    Hit::Browse => act = Act::Browse,
                    Hit::Back => {
                        self.view = View::Main;
                        self.hover = Hit::None;
                        act = Act::Resize;
                    }
                    Hit::AddItem(i) => {
                        if let Some(Entry::App(a)) = self.add_items.get(i).cloned() {
                            self.add_app(a);
                        }
                        self.view = View::Main;
                        self.hover = Hit::None;
                        act = Act::Resize;
                    }
                    Hit::Gear => {
                        self.view = View::Settings;
                        self.hover = Hit::None;
                        act = Act::Resize;
                    }
                    Hit::Set(h) => act = self.setting(h),
                    Hit::MenuOpen => act = Act::Show,
                    Hit::MenuQuit => act = Act::Quit,
                    Hit::Icon(_) | Hit::Row(_) | Hit::None => {}
                }
                self.invalidate();
                (done, act)
            }
            WM_LBUTTONUP => {
                if self.drag.take().is_some() {
                    settings::save(&self.settings);
                    self.invalidate();
                    return (done, Act::Capture(false));
                }
                (done, Act::None)
            }
            WM_MOUSEWHEEL => {
                let s = self.scale();
                let mut p = POINT { x: lo(lp.0), y: hi(lp.0) };
                unsafe {
                    let _ = ScreenToClient(self.hwnd, &mut p);
                }
                let notches = hi(wp.0 as isize) as f32 / 120.0;
                let (x, y) = (p.x as f32 / s, p.y as f32 / s);
                let n = self.settings.apps.len();
                match (self.view, self.hit_at(x, y)) {
                    // over a slider: the volume, like Windows' sliders
                    (View::Main, Hit::Slider(i)) if self.confirm.is_none() => {
                        if self.cfg(i).1 != Mode::Lock {
                            let v = (self.shown_volume(i) as f32 + notches * 2.0).round().clamp(0.0, 100.0) as u32;
                            self.set_volume(i, v);
                            settings::save(&self.settings);
                        }
                    }
                    // elsewhere: scroll the list (when it's longer than the flyout)
                    (View::Main, _) => SCROLL.with(|c| c.set((c.get() - notches * ROW).clamp(0.0, list_max_scroll(n)))),
                    (View::Add, _) => {
                        let k = self.kinds();
                        ADD_SCROLL.with(|c| c.set((c.get() - notches * ADD_ROW).clamp(0.0, add_max_scroll(&k))));
                    }
                    _ => {}
                }
                let h = self.hit_at(x, y);
                self.hover = h;
                self.invalidate();
                (done, Act::None)
            }
            WM_KEYDOWN if wp.0 == VK_ESCAPE.0 as usize => {
                if self.confirm.take().is_some() {
                    self.invalidate();
                    return (done, Act::None);
                }
                (done, Act::Hide)
            }
            WM_ACTIVATE => {
                if (wp.0 & 0xffff) as u32 == WA_INACTIVE && self.visible && !self.menu_open {
                    return (done, Act::Hide);
                }
                (None, Act::None)
            }
            WM_TIMER if wp.0 == TIMER_ANIM => {
                self.anims.retain(|a| a.1.elapsed() < LOCK_ANIM);
                if self.anims.is_empty() {
                    unsafe {
                        let _ = KillTimer(self.hwnd, TIMER_ANIM);
                    }
                }
                self.invalidate();
                (done, Act::None)
            }
            WM_TIMER if wp.0 == TIMER_TIP => {
                unsafe {
                    let _ = KillTimer(self.hwnd, wp.0);
                }
                self.invalidate();
                (done, Act::None)
            }
            WM_APP_STATUS => {
                self.learn_paths();
                if self.visible {
                    if self.view == View::Add {
                        let n = self.add_items.len();
                        self.refresh_add_items();
                        if n != self.add_items.len() {
                            return (done, Act::Resize);
                        }
                    }
                    self.invalidate();
                }
                (done, Act::None)
            }
            WM_APP_TRAY => {
                let ev = (lp.0 & 0xffff) as u32;
                if ev == NIN_SELECT || ev == NIN_KEYSELECT {
                    return (done, Act::Toggle);
                }
                if ev == WM_CONTEXTMENU {
                    return (done, Act::ShowMenu);
                }
                (done, Act::None)
            }
            WM_APP_SHOW => (done, Act::Show),
            WM_SETTINGCHANGE | WM_DWMCOLORIZATIONCOLORCHANGED | WM_THEMECHANGED => {
                self.apply_theme();
                (None, Act::None)
            }
            WM_DPICHANGED => (done, Act::None),
            WM_DESTROY => {
                self.tray(NIM_DELETE);
                unsafe { PostQuitMessage(0) };
                (done, Act::None)
            }
            _ if msg == self.taskbar_created && msg != 0 => {
                self.tray(NIM_ADD); // Explorer restarted
                (done, Act::None)
            }
            _ => (None, Act::None),
        }
    }
}

fn with_ui<T>(f: impl FnOnce(&mut Ui) -> T) -> Option<T> {
    UI.with(|u| u.try_borrow_mut().ok().and_then(|mut u| u.as_mut().map(f)))
}

fn show(view: View) {
    let Some((hwnd, (x, y, w, h))) = with_ui(|u| {
        u.view = view;
        u.hover = Hit::None;
        u.confirm = None;
        SCROLL.with(|c| c.set(0.0));
        ADD_SCROLL.with(|c| c.set(0.0));
        u.visible = true;
        u.apply_theme();
        (u.hwnd, u.placement())
    }) else {
        return;
    };
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_SHOWWINDOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = InvalidateRect(hwnd, None, false);
    }
}

fn hide() {
    let Some(hwnd) = with_ui(|u| {
        u.visible = false;
        u.notice = false;
        u.closed_menu = u.view == View::Menu;
        // free the drawing surface and icons while hidden (the app idles in the tray)
        u.rt = None;
        u.icons.clear();
        u.closed_at = Instant::now();
        u.drag = None;
        u.confirm = None;
        u.view = View::Main;
        u.hwnd
    }) else {
        return;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

fn resize() {
    let Some((hwnd, (x, y, w, h))) = with_ui(|u| (u.hwnd, u.placement())) else { return };
    unsafe {
        let _ = SetWindowPos(hwnd, HWND_TOPMOST, x, y, w, h, SWP_NOACTIVATE);
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// "Browse for a program…": pick any .exe, also one that isn't running.
fn browse() {
    use windows::Win32::UI::Controls::Dialogs::*;
    let Some(hwnd) = with_ui(|u| {
        u.menu_open = true; // keep the flyout open behind the dialog
        u.hwnd
    }) else {
        return;
    };
    let mut file = [0u16; 1024];
    let filter: Vec<u16> = "Programs (*.exe)\0*.exe\0\0".encode_utf16().collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(file.as_mut_ptr()),
        nMaxFile: file.len() as u32,
        lpstrTitle: w!("Choose a program"),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER,
        ..Default::default()
    };
    let picked = unsafe { GetOpenFileNameW(&mut ofn).as_bool() };
    with_ui(|u| u.menu_open = false);
    if picked {
        let end = file.iter().position(|&c| c == 0).unwrap_or(file.len());
        let path = String::from_utf16_lossy(&file[..end]);
        let exe = audio::stem(&path);
        with_ui(|u| {
            if !exe.is_empty() && !u.settings.apps.iter().any(|a| a.exe == exe) {
                let name = audio::program_name(&path);
                u.add_app(Audible { exe, name, path, volume: u32::MAX });
            }
            u.view = View::Main;
            u.hover = Hit::None;
        });
        resize();
    }
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let r = with_ui(|u| u.handle(msg, wp, lp));
    let (res, act) = r.unwrap_or((None, Act::None));
    match act {
        Act::None => {}
        Act::Toggle => {
            // a click on the icon while the panel is open first closes it by deactivation;
            // after the right-click menu it should open the panel instead
            let (visible, recently) = with_ui(|u| (u.visible, !u.closed_menu && u.closed_at.elapsed() < Duration::from_millis(300))).unwrap_or((false, false));
            if visible {
                hide();
            } else if !recently {
                show(View::Main); // (a click on the icon while open first closes it by deactivation)
            }
        }
        Act::Show => show(View::Main),
        Act::ShowMenu => show(View::Menu),
        Act::Quit => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        Act::Hide => hide(),
        Act::Resize => resize(),
        Act::Browse => browse(),
        Act::Capture(on) => unsafe {
            if on {
                SetCapture(hwnd);
            } else {
                let _ = ReleaseCapture();
            }
        },
    }
    res.unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, wp, lp) })
}

pub fn run() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let args: Vec<String> = std::env::args().collect();
        let has = |a: &str| args.iter().any(|x| x == a);
        if has(startup::UNINSTALL_ARG) {
            startup::uninstall(CLASS);
            return;
        }
        // started from a download: install (or update) and let the installed copy run
        let installed = if cfg!(debug_assertions) {
            false // development builds run from where they are and register nothing
        } else {
            match startup::install_if_needed(CLASS) {
                startup::Install::Started => return,
                startup::Install::Here => true,
                startup::Install::Failed => false,
            }
        };
        let _mutex = CreateMutexW(None, true, w!("Local\\AppVolumeLock.Instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // already running: open its flyout instead
            if let Ok(h) = FindWindowW(CLASS, None) {
                let _ = PostMessageW(h, WM_APP_SHOW, WPARAM(0), LPARAM(0));
            }
            return;
        }
        let settings = settings::load();
        SHOW_SYS.store(settings.show_system_sounds, Relaxed);
        // only the installed copy points the Start menu, Settings → Apps and sign-in at itself
        if installed {
            startup::set(settings.start_with_windows);
            startup::register();
        }
        let at_sign_in = has(startup::STARTUP_ARG);
        let just_installed = has(startup::INSTALLED_ARG);
        {
            let mut s = SHARED.lock().unwrap();
            s.apps = settings.apps.clone();
            s.system = settings.system_sounds;
            s.all_devices = settings.all_devices;
        }

        let Ok(hinst) = GetModuleHandleW(None) else { return };
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: LoadIconW(hinst, PCWSTR(1 as _)).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(WS_EX_TOOLWINDOW | WS_EX_TOPMOST, CLASS, w!("App Volume Lock"), WS_POPUP, 0, 0, 10, 10, None, None, hinst, None)
        else {
            return;
        };
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as _, 4);
        let _ = DwmExtendFrameIntoClientArea(hwnd, &MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 });

        let Ok(res) = Res::new() else { return };
        let ui = Ui {
            hwnd,
            res,
            rt: None,
            icons: HashMap::new(),
            settings,
            theme: theme::read(),
            backdrop: false,
            view: View::Main,
            hover: Hit::None,
            hover_at: Instant::now(),
            drag: None,
            confirm: None,
            tracking: false,
            anims: vec![],
            add_items: vec![],
            open_apps: vec![],
            visible: false,
            closed_at: Instant::now() - Duration::from_secs(1),
            menu_open: false,
            notice: false,
            closed_menu: false,
            icon: HICON::default(),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        };
        UI.with(|u| *u.borrow_mut() = Some(ui));
        with_ui(|u| {
            u.apply_theme();
            u.tray(NIM_ADD);
        });
        audio::NOTIFY_HWND.store(hwnd.0 as isize, Relaxed);
        audio::start();
        // opened by hand (not at sign-in): show the flyout, so it's clear the app is running
        if !at_sign_in {
            if just_installed {
                with_ui(|u| u.notice = true);
            }
            show(View::Main);
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
