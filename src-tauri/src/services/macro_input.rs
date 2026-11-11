//! Native input for macros on Windows: synthesis through `SendInput`
//! (`createWindowsInputHandler`) and capture through low-level keyboard and
//! mouse hooks (`createWindowsRecorder` in `recorder-worker.js`).

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::LazyLock;

use parking_lot::Mutex;

/// Every key name a macro may use (`MACRO_NAMED_KEYS`).
pub const MACRO_NAMED_KEYS: &[&str] = &[
    "backspace", "delete", "enter", "tab", "escape", "up", "down", "right", "left", "home", "end", "pageup", "pagedown", "f1", "f2", "f3", "f4",
    "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12", "f13", "f14", "f15", "f16", "f17", "f18", "f19", "f20", "f21", "f22", "f23", "f24",
    "command", "alt", "right_alt", "control", "right_control", "shift", "right_shift", "space", "printscreen", "insert", "menu", "audio_mute",
    "audio_vol_down", "audio_vol_up", "audio_play", "audio_stop", "audio_pause", "audio_prev", "audio_next", "audio_rewind", "audio_forward",
    "audio_repeat", "audio_random", "numpad_lock", "numpad_0", "numpad_1", "numpad_2", "numpad_3", "numpad_4", "numpad_5", "numpad_6",
    "numpad_7", "numpad_8", "numpad_9", "numpad_+", "numpad_-", "numpad_*", "numpad_/", "numpad_.", "lights_mon_up", "lights_mon_down",
    "lights_kbd_toggle", "lights_kbd_up", "lights_kbd_down",
];

/// `WINDOWS_NAMED_KEY_TO_VK`.
pub const NAMED_KEY_TO_VK: &[(&str, u16)] = &[
    ("backspace", 8), ("tab", 9), ("enter", 13), ("shift", 16), ("control", 17), ("alt", 18), ("escape", 27), ("space", 32), ("pageup", 33),
    ("pagedown", 34), ("end", 35), ("home", 36), ("left", 37), ("up", 38), ("right", 39), ("down", 40), ("printscreen", 44), ("insert", 45),
    ("delete", 46), ("command", 91), ("menu", 93), ("numpad_0", 96), ("numpad_1", 97), ("numpad_2", 98), ("numpad_3", 99), ("numpad_4", 100),
    ("numpad_5", 101), ("numpad_6", 102), ("numpad_7", 103), ("numpad_8", 104), ("numpad_9", 105), ("numpad_*", 106), ("numpad_+", 107),
    ("numpad_-", 109), ("numpad_.", 110), ("numpad_/", 111), ("f1", 112), ("f2", 113), ("f3", 114), ("f4", 115), ("f5", 116), ("f6", 117),
    ("f7", 118), ("f8", 119), ("f9", 120), ("f10", 121), ("f11", 122), ("f12", 123), ("f13", 124), ("f14", 125), ("f15", 126), ("f16", 127),
    ("f17", 128), ("f18", 129), ("f19", 130), ("f20", 131), ("f21", 132), ("f22", 133), ("f23", 134), ("f24", 135), ("numpad_lock", 144),
    ("right_shift", 161), ("right_control", 163), ("right_alt", 165), ("audio_mute", 173), ("audio_vol_down", 174), ("audio_vol_up", 175),
    ("audio_next", 176), ("audio_prev", 177), ("audio_stop", 178), ("audio_play", 179), ("audio_pause", 179),
];

const EXTENDED_VK_CODES: [u16; 22] = [33, 34, 35, 36, 37, 38, 39, 40, 44, 45, 46, 91, 93, 111, 144, 163, 165, 173, 174, 175, 176, 177];
const EXTENDED_VK_EXTRA: [u16; 2] = [178, 179];

/// `WINDOWS_OEM_VK_TO_CHAR`.
pub const OEM_VK_TO_CHAR: &[(u16, char)] =
    &[(186, ';'), (187, '='), (188, ','), (189, '-'), (190, '.'), (191, '/'), (192, '`'), (219, '['), (220, '\\'), (221, ']'), (222, '\'')];

/// `WINDOWS_LEFT_MODIFIER_VK_TO_NAME`.
pub const LEFT_MODIFIER_VK_TO_NAME: &[(u16, &str)] = &[(160, "shift"), (162, "control"), (164, "alt")];

fn is_extended(vk: u16) -> bool {
    EXTENDED_VK_CODES.contains(&vk) || EXTENDED_VK_EXTRA.contains(&vk)
}

/// `resolveVk`.
fn resolve_vk(key: &str) -> Result<u16, String> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if let Some((vk, _)) = OEM_VK_TO_CHAR.iter().find(|(_, ch)| *ch == c) {
            return Ok(*vk);
        }
        let upper = c.to_ascii_uppercase() as u32;
        if (48..=57).contains(&upper) || (65..=90).contains(&upper) {
            return Ok(upper as u16);
        }
        #[cfg(windows)]
        {
            let mut buf = [0u16; 2];
            let unit = c.encode_utf16(&mut buf)[0];
            let scanned = unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(unit) };
            if scanned == -1 {
                return Err(format!("Character \"{key}\" is not available on the current keyboard layout"));
            }
            return Ok((scanned as u16) & 255);
        }
        #[cfg(not(windows))]
        return Err(format!("Character \"{key}\" is not available on the current keyboard layout"));
    }
    match NAMED_KEY_TO_VK.iter().find(|(name, _)| *name == key) {
        Some((_, vk)) => Ok(*vk),
        None if MACRO_NAMED_KEYS.contains(&key) => Err(format!("Key \"{key}\" is not supported on Windows")),
        None => Err(format!("Unknown key \"{key}\"")),
    }
}

#[cfg(windows)]
mod win {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

    pub fn key_input(vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
    }

    pub fn mouse_input(flags: u32, data: i32) -> INPUT {
        INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
    }

    pub fn send(events: &[INPUT]) -> Result<(), String> {
        if events.is_empty() {
            return Ok(());
        }
        let injected = unsafe { SendInput(events.len() as u32, events.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
        if injected as usize != events.len() {
            return Err(format!("SendInput injected {injected} of {} events", events.len()));
        }
        Ok(())
    }

    pub fn scan_code(vk: u16) -> u16 {
        unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) as u16 }
    }
}

#[cfg(windows)]
fn key_event(key: &str, up: bool) -> Result<windows_sys::Win32::UI::Input::KeyboardAndMouse::INPUT, String> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE};
    let vk = resolve_vk(key)?;
    let scan = win::scan_code(vk);
    let extended = if is_extended(vk) { KEYEVENTF_EXTENDEDKEY } else { 0 };
    let up_flag = if up { KEYEVENTF_KEYUP } else { 0 };
    Ok(if scan == 0 { win::key_input(vk, 0, extended | up_flag) } else { win::key_input(0, scan, KEYEVENTF_SCANCODE | extended | up_flag) })
}

/// `keyToggle`: modifiers go down first and come up last.
pub fn key_toggle(key: &str, down: bool, modifiers: &[String]) -> Result<(), String> {
    #[cfg(windows)]
    {
        let mut events = Vec::new();
        if down {
            for m in modifiers {
                events.push(key_event(m, false)?);
            }
            events.push(key_event(key, false)?);
        } else {
            events.push(key_event(key, true)?);
            for m in modifiers.iter().rev() {
                events.push(key_event(m, true)?);
            }
        }
        win::send(&events)
    }
    #[cfg(not(windows))]
    {
        let _ = (key, down, modifiers);
        Err("Input automation is unavailable on this system".into())
    }
}

#[cfg(windows)]
fn button_flags(button: &str) -> (u32, u32) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    match button {
        "right" => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        "middle" => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        _ => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
    }
}

pub fn mouse_click(button: &str, double: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let (down, up) = button_flags(button);
        let once = [win::mouse_input(down, 0), win::mouse_input(up, 0)];
        let events: Vec<_> = if double { once.iter().chain(once.iter()).copied().collect() } else { once.to_vec() };
        win::send(&events)
    }
    #[cfg(not(windows))]
    {
        let _ = (button, double);
        Err("Input automation is unavailable on this system".into())
    }
}

pub fn mouse_toggle(down: bool, button: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        let (d, u) = button_flags(button);
        win::send(&[win::mouse_input(if down { d } else { u }, 0)])
    }
    #[cfg(not(windows))]
    {
        let _ = (down, button);
        Err("Input automation is unavailable on this system".into())
    }
}

/// `scrollMouse(0, clicks)`: positive scrolls up.
pub fn scroll(clicks: i32) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::MOUSEEVENTF_WHEEL;
        if clicks == 0 {
            return Ok(());
        }
        win::send(&[win::mouse_input(MOUSEEVENTF_WHEEL, clicks * 120)])
    }
    #[cfg(not(windows))]
    {
        let _ = clicks;
        Err("Input automation is unavailable on this system".into())
    }
}

/// `typeString`: one KEYEVENTF_UNICODE down/up pair per UTF-16 unit.
pub fn type_string(text: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{KEYEVENTF_KEYUP, KEYEVENTF_UNICODE};
        let events: Vec<_> =
            text.encode_utf16().flat_map(|unit| [win::key_input(0, unit, KEYEVENTF_UNICODE), win::key_input(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)]).collect();
        win::send(&events)
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        Err("Input automation is unavailable on this system".into())
    }
}

// --------------------------------------------------------------- recorder

/// One captured input (`RecorderRawEvent`).
#[derive(Clone, Debug)]
pub enum RawEvent {
    Key { down: bool, vk: u16, t_ms: f64 },
    Mouse { down: bool, button: u8, t_ms: f64 },
    Wheel { rotation: f64, t_ms: f64 },
}

pub enum HookMessage {
    Event(RawEvent),
    Escape,
}

static HOOK_SINK: LazyLock<Mutex<Option<mpsc::Sender<HookMessage>>>> = LazyLock::new(|| Mutex::new(None));

#[cfg(windows)]
mod hooks {
    use super::*;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    fn send(message: HookMessage) {
        if let Some(sink) = HOOK_SINK.lock().as_ref() {
            let _ = sink.send(message);
        }
    }

    pub unsafe extern "system" fn keyboard(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let data = &*(lparam as *const KBDLLHOOKSTRUCT);
            if data.flags & LLKHF_INJECTED == 0 {
                let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
                if data.vkCode == 27 {
                    if down {
                        send(HookMessage::Escape);
                    }
                } else {
                    send(HookMessage::Event(RawEvent::Key { down, vk: data.vkCode as u16, t_ms: data.time as f64 }));
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    pub unsafe extern "system" fn mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 {
            let data = &*(lparam as *const MSLLHOOKSTRUCT);
            if data.flags & LLMHF_INJECTED == 0 {
                let t_ms = data.time as f64;
                let button = match wparam as u32 {
                    WM_LBUTTONDOWN => Some((0, true)),
                    WM_LBUTTONUP => Some((0, false)),
                    WM_RBUTTONDOWN => Some((1, true)),
                    WM_RBUTTONUP => Some((1, false)),
                    WM_MBUTTONDOWN => Some((2, true)),
                    WM_MBUTTONUP => Some((2, false)),
                    _ => None,
                };
                if let Some((button, down)) = button {
                    send(HookMessage::Event(RawEvent::Mouse { down, button, t_ms }));
                } else if wparam as u32 == WM_MOUSEWHEEL {
                    let delta = (data.mouseData >> 16) as u16 as i16;
                    if delta != 0 {
                        send(HookMessage::Event(RawEvent::Wheel { rotation: delta as f64 / 120.0, t_ms }));
                    }
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

/// A running pair of low-level hooks on its own message-loop thread.
pub struct HookRecorder {
    thread_id: u32,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl HookRecorder {
    /// Installs the hooks; messages arrive on `sink` until [`stop`].
    pub fn start(sink: mpsc::Sender<HookMessage>) -> Result<Self, String> {
        *HOOK_SINK.lock() = Some(sink);
        #[cfg(windows)]
        {
            let (ready_tx, ready_rx) = mpsc::channel();
            let handle = std::thread::spawn(move || unsafe {
                use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
                use windows_sys::Win32::System::Threading::GetCurrentThreadId;
                use windows_sys::Win32::UI::WindowsAndMessaging::*;
                let module = GetModuleHandleW(std::ptr::null());
                let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hooks::keyboard), module, 0);
                let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(hooks::mouse), module, 0);
                if keyboard.is_null() || mouse.is_null() {
                    if !keyboard.is_null() {
                        UnhookWindowsHookEx(keyboard);
                    }
                    if !mouse.is_null() {
                        UnhookWindowsHookEx(mouse);
                    }
                    let _ = ready_tx.send(Err("SetWindowsHookExW failed".to_owned()));
                    return;
                }
                let _ = ready_tx.send(Ok(GetCurrentThreadId()));
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                UnhookWindowsHookEx(keyboard);
                UnhookWindowsHookEx(mouse);
            });
            let thread_id = ready_rx.recv().map_err(|e| e.to_string())??;
            Ok(Self { thread_id, handle: Some(handle) })
        }
        #[cfg(not(windows))]
        Err("Input recording is not supported on this platform".into())
    }

    pub fn stop(mut self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(self.thread_id, windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT, 0, 0);
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        *HOOK_SINK.lock() = None;
    }
}

/// `WINDOWS_VK_TO_NAME` + digits/letters/OEM (`resolveKeyName`).
pub fn key_name(vk: u16) -> Option<String> {
    static TABLE: LazyLock<HashMap<u16, &'static str>> = LazyLock::new(|| {
        let mut table = HashMap::new();
        for (name, vk) in NAMED_KEY_TO_VK {
            table.entry(*vk).or_insert(*name);
        }
        for (vk, name) in LEFT_MODIFIER_VK_TO_NAME {
            table.entry(*vk).or_insert(*name);
        }
        table
    });
    if let Some(name) = TABLE.get(&vk) {
        return Some((*name).to_owned());
    }
    if (48..=57).contains(&vk) {
        return Some(char::from(vk as u8).to_string());
    }
    if (65..=90).contains(&vk) {
        return Some(char::from(vk as u8).to_ascii_lowercase().to_string());
    }
    OEM_VK_TO_CHAR.iter().find(|(v, _)| *v == vk).map(|(_, c)| c.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn key_tables() {
        assert_eq!(super::key_name(65).as_deref(), Some("a"));
        assert_eq!(super::key_name(160).as_deref(), Some("shift"));
        assert_eq!(super::key_name(16).as_deref(), Some("shift"));
        assert_eq!(super::key_name(179).as_deref(), Some("audio_play"));
        assert_eq!(super::resolve_vk("f5"), Ok(116));
        assert!(super::resolve_vk("lights_kbd_up").is_err());
    }
}
