// The volume worker. It only does what the Volume Mixer does: it sets an app's own
// audio session volume / mute (ISimpleAudioVolume) through Windows' audio API.
// Windows' overall volume is never touched.
//
// A locked app is put back at once: Windows tells us when a session appears
// (IAudioSessionNotification) or a watched session's volume changes
// (IAudioSessionEvents). Those callbacks only wake this thread (setting a volume
// from inside them isn't allowed); a 2 s check is the fallback. Every active output
// device is watched; the list is refreshed when a device changes and every 10 s.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_APP};

use crate::settings::{self, AppCfg, SystemSoundsCfg, Mode};

pub const WM_APP_STATUS: u32 = WM_APP + 1;
/// key used in `once` / `unmute` for Windows' system sounds
pub const SYSTEM: &str = "";

#[derive(Clone, PartialEq, Debug)]
pub struct Audible {
    pub exe: String,
    pub name: String,
    pub path: String,
    pub volume: u32,
}

#[derive(Default)]
pub struct Shared {
    pub apps: Vec<AppCfg>,
    /// "System sounds"
    pub system: SystemSoundsCfg,
    /// lock on every output device, or only on the default one
    pub all_devices: bool,
    /// slider just moved on an unlocked app (or SYSTEM): set its volume once
    pub once: HashSet<String>,
    /// just left mute mode: unmute once
    pub unmute: HashSet<String>,
    /// per listed app: (running, current volume %)
    pub status: HashMap<String, (bool, u32)>,
    /// per listed app: its program file, when running
    pub paths: HashMap<String, String>,
    /// current system sounds volume %
    pub system_now: Option<u32>,
    /// apps with sound right now, as the Volume Mixer would list them
    pub audible: Vec<Audible>,
}

pub static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared::default()));
pub static NOTIFY_HWND: AtomicIsize = AtomicIsize::new(0);
static WAKE: AtomicIsize = AtomicIsize::new(0);

pub fn wake() {
    let h = WAKE.load(Relaxed);
    if h != 0 {
        unsafe {
            let _ = SetEvent(HANDLE(h as _));
        }
    }
}

pub fn start() {
    let ev = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }.expect("event");
    WAKE.store(ev.0 as isize, Relaxed);
    std::thread::spawn(|| unsafe { worker() });
}

#[implement(IAudioSessionNotification)]
struct SessionNotify;

impl IAudioSessionNotification_Impl for SessionNotify_Impl {
    fn OnSessionCreated(&self, _new: Option<&IAudioSessionControl>) -> Result<()> {
        wake();
        Ok(())
    }
}

#[implement(IAudioSessionEvents)]
struct VolumeEvents;

impl IAudioSessionEvents_Impl for VolumeEvents_Impl {
    fn OnDisplayNameChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnIconPathChanged(&self, _: &PCWSTR, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnSimpleVolumeChanged(&self, _: f32, _: BOOL, _: *const GUID) -> Result<()> {
        wake();
        Ok(())
    }
    fn OnChannelVolumeChanged(&self, _: u32, _: *const f32, _: u32, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnGroupingParamChanged(&self, _: *const GUID, _: *const GUID) -> Result<()> {
        Ok(())
    }
    fn OnStateChanged(&self, _: AudioSessionState) -> Result<()> {
        wake();
        Ok(())
    }
    fn OnSessionDisconnected(&self, _: AudioSessionDisconnectReason) -> Result<()> {
        wake();
        Ok(())
    }
}

/// An output device was plugged in, removed or made default (headphones, AirPods …):
/// look at the devices again right away instead of waiting for the 10 s check.
static DEVICES_CHANGED: AtomicBool = AtomicBool::new(false);

#[implement(IMMNotificationClient)]
struct DeviceEvents;

impl DeviceEvents {
    fn changed() -> Result<()> {
        DEVICES_CHANGED.store(true, Relaxed);
        wake();
        Ok(())
    }
}

impl IMMNotificationClient_Impl for DeviceEvents_Impl {
    fn OnDeviceStateChanged(&self, _: &PCWSTR, _: DEVICE_STATE) -> Result<()> {
        DeviceEvents::changed()
    }
    fn OnDeviceAdded(&self, _: &PCWSTR) -> Result<()> {
        DeviceEvents::changed()
    }
    fn OnDeviceRemoved(&self, _: &PCWSTR) -> Result<()> {
        DeviceEvents::changed()
    }
    fn OnDefaultDeviceChanged(&self, _: EDataFlow, _: ERole, _: &PCWSTR) -> Result<()> {
        DeviceEvents::changed()
    }
    fn OnPropertyValueChanged(&self, _: &PCWSTR, _: &windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY) -> Result<()> {
        Ok(())
    }
}

struct Device {
    id: String,
    mgr: IAudioSessionManager2,
}

unsafe fn pwstr_take(p: PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as _));
    s
}

/// Active output devices: (id, device)
unsafe fn render_devices(en: &IMMDeviceEnumerator) -> Result<Vec<(String, IMMDevice)>> {
    let col = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    let mut out = vec![];
    for i in 0..col.GetCount()? {
        let d = col.Item(i)?;
        out.push((pwstr_take(d.GetId()?), d));
    }
    Ok(out)
}

pub fn stem(path: &str) -> String {
    let file = path.rsplit(['\\', '/']).next().unwrap_or(path).to_lowercase();
    file.strip_suffix(".exe").unwrap_or(&file).to_string()
}

unsafe fn image_path(pid: u32) -> Option<String> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
    let _ = CloseHandle(h);
    ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// The Store-app id of a packaged process (none for ordinary programs).
unsafe fn app_id(pid: u32) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    let r = GetApplicationUserModelId(h, &mut len, PWSTR(buf.as_mut_ptr()));
    let _ = CloseHandle(h);
    if r != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let s = String::from_utf16_lossy(&buf[..end]);
    (!s.is_empty()).then_some(s)
}

unsafe fn parent_pids() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
    let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    if Process32FirstW(snap, &mut e).is_ok() {
        loop {
            map.insert(e.th32ProcessID, e.th32ParentProcessID);
            if Process32NextW(snap, &mut e).is_err() {
                break;
            }
        }
    }
    let _ = CloseHandle(snap);
    map
}

/// Where a listed app's program is when it isn't playing sound (for its icon):
/// a running process of that name, else the usual per-user install folders.
unsafe fn find_program(exe: &str) -> Option<String> {
    if let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut best: Option<(bool, u32)> = None;
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                let name = stem(&String::from_utf16_lossy(&e.szExeFile[..end]));
                if name.starts_with(exe) {
                    let exact = name == exe;
                    if best.map_or(true, |(b, _)| exact && !b) {
                        best = Some((exact, e.th32ProcessID));
                    }
                }
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        if let Some(p) = best.and_then(|(_, pid)| image_path(pid)) {
            return Some(p);
        }
    }
    let local = std::env::var("LOCALAPPDATA").ok()?;
    [format!("{local}\\{exe}\\{exe}.exe"), format!("{local}\\Programs\\{exe}\\{exe}.exe")]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
}

/// The program's own name from its file ("Google Chrome"), like the Volume Mixer shows.
unsafe fn file_description(path: &str) -> Option<String> {
    let p = HSTRING::from(path);
    let size = GetFileVersionInfoSizeW(&p, None);
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    GetFileVersionInfoW(&p, 0, size, data.as_mut_ptr() as _).ok()?;
    let mut ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut len = 0u32;
    let mut langs = vec![];
    if VerQueryValueW(data.as_ptr() as _, w!("\\VarFileInfo\\Translation"), &mut ptr, &mut len).as_bool() && len >= 4 {
        let words = std::slice::from_raw_parts(ptr as *const u16, (len / 2) as usize);
        for pair in words.chunks_exact(2) {
            langs.push(format!("{:04x}{:04x}", pair[0], pair[1]));
        }
    }
    langs.extend(["040904b0".to_string(), "040904e4".to_string()]);
    let read = |field: &str, ptr: &mut *mut core::ffi::c_void, len: &mut u32| -> Option<String> {
        for l in &langs {
            let key = HSTRING::from(format!("\\StringFileInfo\\{l}\\{field}"));
            if VerQueryValueW(data.as_ptr() as _, &key, ptr, len).as_bool() && *len > 0 {
                let raw = std::slice::from_raw_parts(*ptr as *const u16, *len as usize);
                let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
                let s = String::from_utf16_lossy(&raw[..end]).trim().to_string();
                if !s.is_empty() {
                    return Some(s);
                }
            }
        }
        None
    };
    // the product name is what people know ("WhatsApp", not "WhatsApp.Root")
    let product = read("ProductName", &mut ptr, &mut len).filter(|s| !s.contains("Windows") && s.len() <= 40);
    product.or_else(|| read("FileDescription", &mut ptr, &mut len))
}

/// Who a session belongs to, the way the Volume Mixer sees it.
#[derive(Clone)]
struct Ident {
    exe: String,
    name: String,
    /// part of Windows itself (touch keyboard etc.): not offered in "Add an app"
    hidden: bool,
    path: String,
}

struct Names {
    by_pid: HashMap<u32, Option<Ident>>,
    by_exe: HashMap<String, String>,
    parents: Option<HashMap<u32, u32>>,
    windir: String,
}

impl Names {
    fn new() -> Names {
        let windir = std::env::var("windir").unwrap_or_else(|_| "C:\\Windows".into()).to_lowercase();
        Names { by_pid: HashMap::new(), by_exe: HashMap::new(), parents: None, windir }
    }

    fn clear(&mut self) {
        self.by_pid.clear(); // process ids get reused
        self.parents = None;
    }

    unsafe fn get(&mut self, pid: u32) -> Option<Ident> {
        if let Some(i) = self.by_pid.get(&pid) {
            return i.clone();
        }
        let i = self.resolve(pid);
        self.by_pid.insert(pid, i.clone());
        i
    }

    unsafe fn resolve(&mut self, pid: u32) -> Option<Ident> {
        let mut path = image_path(pid)?;
        // Web views (WhatsApp, Teams …) play through msedgewebview2.exe: use the app that owns it
        let mut cur = pid;
        for _ in 0..8 {
            if stem(&path) != "msedgewebview2" {
                break;
            }
            let parents = self.parents.get_or_insert_with(|| parent_pids());
            let Some(&pp) = parents.get(&cur) else { break };
            let Some(pp_path) = image_path(pp) else { break };
            cur = pp;
            path = pp_path;
        }
        let exe = stem(&path);
        let lower = path.to_lowercase();
        let hidden = exe == "msedgewebview2" || lower.starts_with(&self.windir);
        // Store apps keep their logo in the package, not in the .exe: use the app's
        // Start-menu entry for its icon, like the Volume Mixer does
        let icon = app_id(cur).map(|id| format!("shell:AppsFolder\\{id}")).unwrap_or_else(|| path.clone());
        let name = self
            .by_exe
            .entry(exe.clone())
            .or_insert_with(|| {
                let n = file_description(&path).unwrap_or_else(|| settings::display_name(&exe));
                // "WhatsApp.Root" -> "WhatsApp"
                match n.split_once('.') {
                    Some((head, _)) if !n.contains(' ') && !head.is_empty() => head.to_string(),
                    _ => n,
                }
            })
            .clone();
        Some(Ident { exe, name, hidden, path: icon })
    }
}

unsafe fn worker() {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    let ev = HANDLE(WAKE.load(Relaxed) as _);
    let notify: IAudioSessionNotification = SessionNotify.into();
    let events: IAudioSessionEvents = VolumeEvents.into();
    let mut en: Option<IMMDeviceEnumerator> = None;
    let mut devices: Vec<Device> = vec![];
    let mut hooked: HashMap<String, IAudioSessionControl> = HashMap::new();
    let mut names = Names::new();
    let mut found: HashMap<String, String> = HashMap::new();
    let mut checked: Option<Instant> = None;
    let device_events: IMMNotificationClient = DeviceEvents.into();
    loop {
        if en.is_none() {
            en = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL).ok();
            if let Some(e) = &en {
                let _ = e.RegisterEndpointNotificationCallback(&device_events);
            }
        }
        let Some(e) = en.clone() else {
            let _ = WaitForSingleObject(ev, 2000);
            continue;
        };
        if DEVICES_CHANGED.swap(false, Relaxed) {
            checked = None;
        }
        if checked.map_or(true, |t| t.elapsed() > Duration::from_secs(10)) {
            checked = Some(Instant::now());
            names.clear();
            // program files of listed apps that aren't playing, for their icons
            let apps = SHARED.lock().unwrap().apps.clone();
            found.retain(|k, _| apps.iter().any(|a| &a.exe == k));
            for a in &apps {
                if a.path.is_empty() && !found.contains_key(&a.exe) {
                    if let Some(p) = find_program(&a.exe) {
                        found.insert(a.exe.clone(), p);
                    }
                }
            }
            match render_devices(&e) {
                Ok(list) => {
                    let same = list.len() == devices.len() && list.iter().zip(&devices).all(|(a, b)| a.0 == b.id);
                    if !same {
                        for d in &devices {
                            let _ = d.mgr.UnregisterSessionNotification(&notify);
                        }
                        for (_, c) in hooked.drain() {
                            let _ = c.UnregisterAudioSessionNotification(&events);
                        }
                        devices.clear();
                        for (id, dev) in list {
                            let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
                            // Windows only sends "session created" after the list was asked for once
                            let _ = mgr.GetSessionEnumerator();
                            let _ = mgr.RegisterSessionNotification(&notify);
                            devices.push(Device { id, mgr });
                        }
                    }
                }
                Err(_) => {
                    devices.clear();
                    hooked.clear();
                    en = None;
                }
            }
            // forget sessions that are gone for good
            hooked.retain(|_, c| {
                let alive = c.GetState().map_or(false, |s| s != AudioSessionStateExpired);
                if !alive {
                    let _ = c.UnregisterAudioSessionNotification(&events);
                }
                alive
            });
        }
        let default_id = e.GetDefaultAudioEndpoint(eRender, eConsole).ok().and_then(|d| d.GetId().ok()).map(|p| pwstr_take(p));
        apply(&devices, &mut hooked, &mut names, &events, &found, default_id.as_deref());
        let _ = WaitForSingleObject(ev, 2000);
    }
}

/// One entry per app, even when it plays through several programs or sessions
/// (Medal + MedalEncoder, two Chrome sessions …): the Volume Mixer sometimes shows
/// those twice with the same name. A row here covers all of them anyway.
fn merge_duplicates(mut list: Vec<Audible>) -> Vec<Audible> {
    list.sort_by_key(|a| a.exe.len()); // main program first ("medal" before "medalencoder")
    let mut out: Vec<Audible> = vec![];
    for a in list {
        let same = out.iter_mut().find(|o| a.exe.starts_with(&o.exe) || o.name.eq_ignore_ascii_case(&a.name));
        match same {
            Some(o) => o.volume = if o.volume > 100 { a.volume } else if a.volume > 100 { o.volume } else { o.volume.max(a.volume) },
            None => out.push(a),
        }
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// The volume to set for this mode, if any.
fn target(mode: Mode, want: f32, cur: f32, once: bool) -> Option<f32> {
    match mode {
        Mode::Lock if (cur - want).abs() > 0.004 => Some(want),
        Mode::Max if cur > want + 0.004 => Some(want),
        Mode::Off | Mode::Mute if once && (cur - want).abs() > 0.004 => Some(want),
        _ => None,
    }
}

/// If another program keeps setting the same app to a different volume, the two would
/// undo each other endlessly. After 10 corrections within a second, stop for the rest of
/// that second (the 2 s check still holds the lock, without burning CPU).
fn may_correct(key: &str) -> bool {
    static SEEN: LazyLock<Mutex<HashMap<String, (Instant, u32)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut m = SEEN.lock().unwrap();
    let e = m.entry(key.to_string()).or_insert((Instant::now(), 0));
    if e.0.elapsed() > Duration::from_secs(1) {
        *e = (Instant::now(), 0);
    }
    e.1 += 1;
    e.1 <= 10
}

/// Applies one rule to one session; returns its volume afterwards.
unsafe fn enforce(ctl: &IAudioSessionControl, vol: &ISimpleAudioVolume, key: &str, cfg: (u32, Mode), once: &HashSet<String>, unmute: &HashSet<String>, hooked: &mut HashMap<String, IAudioSessionControl>, events: &IAudioSessionEvents) -> Option<f32> {
    // listen to this session's volume so a change is undone at once
    if let Ok(c2) = ctl.cast::<IAudioSessionControl2>() {
        if let Ok(id) = c2.GetSessionInstanceIdentifier() {
            let id = pwstr_take(id);
            if !hooked.contains_key(&id) && ctl.RegisterAudioSessionNotification(events).is_ok() {
                hooked.insert(id, ctl.clone());
            }
        }
    }
    let mut cur = vol.GetMasterVolume().ok()?;
    let want = cfg.0.min(100) as f32 / 100.0;
    if let Some(v) = target(cfg.1, want, cur, once.contains(key)) {
        if may_correct(key) && vol.SetMasterVolume(v, std::ptr::null()).is_ok() {
            cur = v;
        }
    }
    let muted = vol.GetMute().map_or(false, |b| b.as_bool());
    if cfg.1 == Mode::Mute && !muted {
        let _ = vol.SetMute(true, std::ptr::null());
    } else if unmute.contains(key) && muted {
        let _ = vol.SetMute(false, std::ptr::null());
    }
    Some(cur)
}

unsafe fn apply(devices: &[Device], hooked: &mut HashMap<String, IAudioSessionControl>, names: &mut Names, events: &IAudioSessionEvents, found: &HashMap<String, String>, default_id: Option<&str>) {
    let (apps, system, once, unmute, all_devices) = {
        let mut s = SHARED.lock().unwrap();
        (s.apps.clone(), s.system, std::mem::take(&mut s.once), std::mem::take(&mut s.unmute), s.all_devices)
    };
    let mut status: HashMap<String, (bool, u32)> = apps.iter().map(|a| (a.exe.clone(), (false, 0))).collect();
    let mut paths: HashMap<String, String> = HashMap::new();
    let mut system_now: Option<u32> = None;
    let mut audible: BTreeMap<String, Audible> = BTreeMap::new();
    for d in devices {
        // "Default" in Settings: lock only on the device in use; others are just listed
        let here = all_devices || default_id.map_or(true, |id| id == d.id);
        let none = HashSet::new();
        let (once, unmute) = if here { (&once, &unmute) } else { (&none, &none) };
        let rule = |vol: u32, mode: Mode| if here { (vol, mode) } else { (vol, Mode::Off) };
        let Ok(en) = d.mgr.GetSessionEnumerator() else { continue };
        for i in 0..en.GetCount().unwrap_or(0) {
            let Ok(ctl) = en.GetSession(i) else { continue };
            let Ok(ctl2) = ctl.cast::<IAudioSessionControl2>() else { continue };
            if ctl.GetState().map_or(true, |s| s == AudioSessionStateExpired) {
                continue;
            }
            let Ok(vol) = ctl.cast::<ISimpleAudioVolume>() else { continue };

            // Windows' own sounds ("System sounds" in the Volume Mixer)
            if ctl2.IsSystemSoundsSession() == S_OK {
                if let Some(cur) = enforce(&ctl, &vol, SYSTEM, rule(system.volume, system.mode), once, unmute, hooked, events) {
                    let pct = (cur * 100.0).round() as u32;
                    system_now = Some(system_now.map_or(pct, |v| v.max(pct)));
                }
                continue;
            }
            let pid = ctl2.GetProcessId().unwrap_or(0);
            if pid == 0 {
                continue;
            }
            let Some(who) = names.get(pid) else { continue };

            // Matched by the start of the name ("medal" also covers MedalEncoder);
            // the most specific row wins, so a helper can still get a row of its own.
            let cur = if let Some(app) = apps.iter().filter(|a| !a.exe.is_empty() && who.exe.starts_with(&a.exe)).max_by_key(|a| a.exe.len()) {
                let Some(cur) = enforce(&ctl, &vol, &app.exe, rule(app.volume, app.mode), once, unmute, hooked, events) else { continue };
                let e = status.entry(app.exe.clone()).or_default();
                *e = (true, e.1.max((cur * 100.0).round() as u32));
                // the app's main program (not MedalEncoder) for its icon
                if who.exe == app.exe || !paths.contains_key(&app.exe) {
                    paths.insert(app.exe.clone(), who.path.clone());
                }
                cur
            } else {
                vol.GetMasterVolume().unwrap_or(0.0)
            };
            if !who.hidden {
                let pct = (cur * 100.0).round() as u32;
                let a = audible.entry(who.exe.clone()).or_insert(Audible { exe: who.exe, name: who.name, path: who.path, volume: 0 });
                a.volume = a.volume.max(pct);
            }
        }
    }
    for (exe, p) in found {
        paths.entry(exe.clone()).or_insert_with(|| p.clone());
    }
    let audible = merge_duplicates(audible.into_values().collect());
    let changed = {
        let mut s = SHARED.lock().unwrap();
        let changed = s.status != status || s.audible != audible || s.system_now != system_now || s.paths != paths;
        s.status = status;
        s.audible = audible;
        s.system_now = system_now;
        s.paths = paths;
        changed
    };
    if changed {
        let h = NOTIFY_HWND.load(Relaxed);
        if h != 0 {
            let _ = PostMessageW(HWND(h as _), WM_APP_STATUS, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Programs that have played sound on this PC before: Windows keeps their per-app audio
/// settings in its PolicyConfig store (read only).
pub fn sound_history() -> HashSet<String> {
    use windows::Win32::System::Registry::*;
    let mut out = HashSet::new();
    unsafe {
        let mut key = HKEY::default();
        let path = w!("Software\\Microsoft\\Internet Explorer\\LowRegistry\\Audio\\PolicyConfig\\PropertyStore");
        if RegOpenKeyExW(HKEY_CURRENT_USER, path, 0, KEY_READ, &mut key) != ERROR_SUCCESS {
            return out;
        }
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(key, i, PWSTR(name.as_mut_ptr()), &mut len, None, PWSTR::null(), None, None) != ERROR_SUCCESS {
                break;
            }
            let mut buf = [0u16; 2048];
            let mut size = (buf.len() * 2) as u32;
            if RegGetValueW(key, PCWSTR(name.as_ptr()), PCWSTR::null(), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut size)) != ERROR_SUCCESS {
                continue;
            }
            // "{0.0.0.00000000}.{device}|\Device\HarddiskVolumeN\…\app.exe%b{…}"
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            let s = String::from_utf16_lossy(&buf[..end]);
            if let Some((_, rest)) = s.split_once('|') {
                let file = rest.split("%b").next().unwrap_or(rest);
                if file.to_lowercase().ends_with(".exe") {
                    out.insert(stem(file));
                }
            }
        }
        let _ = RegCloseKey(key);
    }
    out
}

/// Apps that are open (own a window, also hidden ones like tray apps) and have played
/// sound before (`known`), even if they aren't playing right now, so Windows doesn't
/// list them in the Volume Mixer. Only real programs: no parts of Windows, no helpers.
pub fn open_apps(known: &HashSet<String>) -> Vec<Audible> {
    unsafe extern "system" fn each(h: HWND, lp: LPARAM) -> BOOL {
        let pids = &mut *(lp.0 as *mut Vec<u32>);
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        pids.push(pid);
        TRUE
    }
    const HELPERS: [&str; 10] = ["helper", "update", "crash", "service", "broker", "container", "elevat", "installer", "collabsync", "feedprovider"];
    unsafe {
        let mut pids: Vec<u32> = vec![];
        let _ = EnumWindows(Some(each), LPARAM(&mut pids as *mut _ as isize));
        pids.sort_unstable();
        pids.dedup();
        let me = GetCurrentProcessId();
        let mut names = Names::new();
        let mut out: BTreeMap<String, Audible> = BTreeMap::new();
        for pid in pids {
            if pid == 0 || pid == me {
                continue;
            }
            let Some(who) = names.get(pid) else { continue };
            if who.hidden || out.contains_key(&who.exe) || !known.contains(&who.exe) || HELPERS.iter().any(|w| who.exe.contains(w)) {
                continue;
            }
            if file_description(&who.path).is_none() || who.name.contains('\\') || who.path.to_lowercase().contains("\\common files\\") {
                continue;
            }
            out.insert(who.exe.clone(), Audible { exe: who.exe, name: who.name, path: who.path, volume: u32::MAX });
        }
        // MedalEncoder belongs to Medal: keep only the main program
        merge_duplicates(out.into_values().collect())
    }
}

/// Name for a program file picked by hand.
pub fn program_name(path: &str) -> String {
    let n = unsafe { file_description(path) }.unwrap_or_else(|| settings::display_name(&stem(path)));
    match n.split_once('.') {
        Some((head, _)) if !n.contains(' ') && !head.is_empty() => head.to_string(),
        _ => n,
    }
}

