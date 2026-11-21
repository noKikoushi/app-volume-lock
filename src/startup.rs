// Installing (per user, no admin rights): the exe copies itself to
// %LOCALAPPDATA%\Programs\App Volume Lock, adds a Start menu shortcut and an entry in
// Settings → Apps (with uninstall). Also "Start with Windows" (HKCU\...\Run).
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Com::{CoCreateInstance, IPersistFile, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::{IShellLinkW, ShellExecuteW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, MessageBoxW, PostMessageW, IDYES, MB_ICONQUESTION, MB_YESNO, SW_SHOWNORMAL, WM_CLOSE};

const RUN: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const UNINSTALL: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\AppVolumeLock";
const NAME: PCWSTR = w!("App Volume Lock");

/// Passed when Windows starts the app at sign-in: stay quietly in the tray.
pub const STARTUP_ARG: &str = "--startup";
/// Passed to the freshly installed copy: say so in the panel.
pub const INSTALLED_ARG: &str = "--installed";
/// Used by Settings → Apps → Uninstall.
pub const UNINSTALL_ARG: &str = "--uninstall";

pub fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("LOCALAPPDATA").unwrap_or_default()).join("Programs\\App Volume Lock")
}

fn installed_exe() -> PathBuf {
    install_dir().join("AppVolumeLock.exe")
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Asks a running copy (by its window class) to quit, and waits until it has.
pub fn close_running(class: PCWSTR) {
    unsafe {
        let Ok(h) = FindWindowW(class, None) else { return };
        let _ = PostMessageW(h, WM_CLOSE, WPARAM(0), LPARAM(0));
        let t0 = Instant::now();
        while FindWindowW(class, None).is_ok() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub enum Install {
    /// this is the installed copy: carry on normally
    Here,
    /// the installed copy was (re)placed and started: this one should exit
    Started,
    /// couldn't install: run from where we are, but don't register this location
    Failed,
}

/// Run from anywhere else (Downloads …): copy this exe into place and start that copy.
/// Running a newer download updates the installed copy.
pub fn install_if_needed(class: PCWSTR) -> Install {
    let Ok(me) = std::env::current_exe() else { return Install::Failed };
    let target = installed_exe();
    if same_file(&me, &target) {
        return Install::Here;
    }
    close_running(class);
    if std::fs::create_dir_all(install_dir()).is_err() {
        return Install::Failed;
    }
    // Copy next to it first, then swap it in with one rename, so a failed copy (disk
    // full …) never leaves a broken exe behind. The old copy may need a moment to let go.
    let fresh = target.with_extension("exe.new");
    let t0 = Instant::now();
    while std::fs::copy(&me, &fresh).is_err() || std::fs::rename(&fresh, &target).is_err() {
        if t0.elapsed() > Duration::from_secs(5) {
            let _ = std::fs::remove_file(&fresh);
            return Install::Failed;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let started = unsafe { ShellExecuteW(None, w!("open"), &HSTRING::from(target.as_os_str()), &HSTRING::from(INSTALLED_ARG), None, SW_SHOWNORMAL) };
    // ShellExecute reports success with a value above 32
    if started.0 as isize > 32 { Install::Started } else { Install::Failed }
}

fn set_value(key: &str, name: &str, value: &str) {
    let data: Vec<u16> = value.encode_utf16().chain([0]).collect();
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name), REG_SZ.0, Some(data.as_ptr() as *const _), (data.len() * 2) as u32);
    }
}

fn set_dword(key: &str, name: &str, value: u32) {
    unsafe {
        let _ = RegSetKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name), REG_DWORD.0, Some(&value as *const u32 as *const _), 4);
    }
}

/// The Start menu shortcut and the Settings → Apps entry, pointing at this exe.
pub fn register() {
    let Ok(exe) = std::env::current_exe() else { return };
    let exe_s = exe.display().to_string();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let lnk = PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\App Volume Lock.lnk");
        unsafe {
            if let Ok(link) = CoCreateInstance::<_, IShellLinkW>(&ShellLink, None, CLSCTX_INPROC_SERVER) {
                let _ = link.SetPath(&HSTRING::from(exe.as_os_str()));
                let _ = link.SetDescription(w!("Keeps app volumes where you set them"));
                if let Ok(file) = link.cast::<IPersistFile>() {
                    let _ = file.Save(&HSTRING::from(lnk.as_os_str()), true);
                }
            }
        }
    }
    set_value(UNINSTALL, "DisplayName", "App Volume Lock");
    set_value(UNINSTALL, "DisplayVersion", env!("CARGO_PKG_VERSION"));
    set_value(UNINSTALL, "Publisher", "nokikoushi");
    set_value(UNINSTALL, "DisplayIcon", &exe_s);
    set_value(UNINSTALL, "InstallLocation", &install_dir().display().to_string());
    set_value(UNINSTALL, "UninstallString", &format!("\"{exe_s}\" {UNINSTALL_ARG}"));
    set_value(UNINSTALL, "URLInfoAbout", "https://github.com/noKikoushi/app-volume-lock");
    set_dword(UNINSTALL, "NoModify", 1);
    set_dword(UNINSTALL, "NoRepair", 1);
    let kb = std::fs::metadata(&exe).map_or(0, |m| (m.len() / 1024) as u32);
    set_dword(UNINSTALL, "EstimatedSize", kb);
}

/// Settings → Apps → Uninstall: asks once, then removes everything this app created.
pub fn uninstall(class: PCWSTR) {
    unsafe {
        let ask = MessageBoxW(HWND::default(), w!("Remove App Volume Lock and its settings from this PC?"), NAME, MB_YESNO | MB_ICONQUESTION);
        if ask != IDYES {
            return;
        }
    }
    close_running(class);
    set(false);
    unsafe {
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(UNINSTALL));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let _ = std::fs::remove_file(PathBuf::from(appdata).join("Microsoft\\Windows\\Start Menu\\Programs\\App Volume Lock.lnk"));
    }
    let _ = std::fs::remove_dir_all(crate::settings::dir());
    // A running exe can't be deleted, but it can be moved: put it in the temp folder
    // (Windows clears that on its own), then the program folder is empty and goes too.
    let dir = install_dir();
    if let Ok(me) = std::env::current_exe() {
        if me.starts_with(&dir) {
            let parked = std::env::temp_dir().join(format!("AppVolumeLock-removed-{}.exe", std::process::id()));
            let _ = std::fs::rename(&me, parked);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

pub fn set(enable: bool) {
    unsafe {
        if enable {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd: Vec<u16> = format!("\"{}\" {STARTUP_ARG}", exe.display()).encode_utf16().chain([0]).collect();
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, RUN, NAME, REG_SZ.0, Some(cmd.as_ptr() as *const _), (cmd.len() * 2) as u32);
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, NAME);
        }
    }
}

pub fn is_set() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, NAME, RRF_RT_REG_SZ, None, None, None).is_ok() }
}
