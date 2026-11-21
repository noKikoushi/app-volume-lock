// Light / dark and the Windows accent colour, read from the user's personalisation settings.
use windows::core::{w, PCWSTR};
use windows::Win32::System::Registry::*;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub dark: bool,
    pub taskbar_dark: bool,
    /// accent tuned for the theme (Windows uses a lighter shade on dark, a darker one on light)
    pub accent: [f32; 3],
}

const PERSONALIZE: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
const ACCENT: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Accent");

fn dword(key: PCWSTR, name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size))
            .ok()
            .ok()?;
    }
    Some(v)
}

fn palette() -> Option<[u8; 32]> {
    let mut buf = [0u8; 32];
    let mut size = 32u32;
    unsafe {
        RegGetValueW(HKEY_CURRENT_USER, ACCENT, w!("AccentPalette"), RRF_RT_REG_BINARY, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size))
            .ok()
            .ok()?;
    }
    (size >= 32).then_some(buf)
}

fn rgb(p: &[u8; 32], i: usize) -> [f32; 3] {
    [p[i * 4] as f32 / 255.0, p[i * 4 + 1] as f32 / 255.0, p[i * 4 + 2] as f32 / 255.0]
}

/// The flyout is always dark (like Windows' own flyouts look best); the tray icon still
/// follows the taskbar's colour.
pub fn read() -> Theme {
    let dark = true;
    let taskbar_dark = dword(PERSONALIZE, w!("SystemUsesLightTheme")).map_or(true, |v| v == 0);
    // AccentPalette: Light3, Light2, Light1, Base, Dark1, Dark2, Dark3, (unused)
    let accent = match palette() {
        Some(p) if dark => rgb(&p, 1),
        Some(p) => rgb(&p, 4),
        None if dark => [0x4c as f32 / 255.0, 0xc2 as f32 / 255.0, 1.0],
        None => [0.0, 0x5f as f32 / 255.0, 0xb8 as f32 / 255.0],
    };
    Theme { dark, taskbar_dark, accent }
}
