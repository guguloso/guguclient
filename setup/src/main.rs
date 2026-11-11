#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use flate2::read::DeflateDecoder;
use windows::core::{Interface, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Globalization::GetUserDefaultLocaleName;
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWCP_ROUND, DWMWA_USE_IMMERSIVE_DARK_MODE,
    DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC, InvalidateRect,
    LineTo, MoveToEx, ReleaseDC, RoundRect, SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    DT_CENTER, DT_END_ELLIPSIS, FF_SWISS, FW_BOLD, FW_NORMAL, FW_SEMIBOLD, HFONT,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, IPersistFile,
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::System::Threading::{
    OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_Programs,
    IShellLinkW, SHGetKnownFolderPath, ShellLink, KNOWN_FOLDER_FLAG,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, DrawIconEx, GetClientRect,
    GetMessageW, GetSystemMetrics, LoadCursorW, LoadIconW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, DI_NORMAL, HICON, IDC_ARROW,
    IDC_HAND, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_NOZORDER, SW_SHOW, WM_APP, WM_CLOSE,
    WM_CREATE, WM_DESTROY, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WM_SETCURSOR, WNDCLASSW,
    WS_CAPTION, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_SYSMENU,
};

const PAYLOAD_DEFLATE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/GuguClient.deflate"));
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const UNINSTALL_REG_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\GuguClient";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const WM_APP_PROGRESS: u32 = WM_APP + 1;
const WM_APP_DONE: u32 = WM_APP + 2;

const WIN_W: i32 = 468;
const WIN_H: i32 = 342;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    PtBr,
    Es,
    En,
}

struct Strings {
    window_title: &'static str,
    subtitle: &'static str,
    install_dir_label: &'static str,
    cb_desktop: &'static str,
    cb_start_menu: &'static str,
    cb_launch: &'static str,
    cb_remove_data: &'static str,
    uninstall_desc: &'static str,
    btn_install: &'static str,
    btn_update: &'static str,
    btn_uninstall: &'static str,
    btn_cancel: &'static str,
    btn_close: &'static str,
    status_ready: &'static str,
    status_ready_update: &'static str,
    status_stopping: &'static str,
    status_extracting: &'static str,
    status_shortcuts: &'static str,
    status_registering: &'static str,
    status_done_install: &'static str,
    status_removing: &'static str,
    status_done_uninstall: &'static str,
    status_error: &'static str,
}

fn strings(lang: Lang) -> &'static Strings {
    match lang {
        Lang::PtBr => &Strings {
            window_title: "Gugu Client Setup",
            subtitle: "Instalador nativo em Rust",
            install_dir_label: "Pasta de instalação:",
            cb_desktop: "Criar atalho na Área de Trabalho",
            cb_start_menu: "Criar atalho no Menu Iniciar",
            cb_launch: "Abrir o Gugu Client ao concluir",
            cb_remove_data: "Remover contas salvas, instâncias e configurações",
            uninstall_desc: "Isso removerá o Gugu Client e seus atalhos deste computador.",
            btn_install: "Instalar",
            btn_update: "Atualizar",
            btn_uninstall: "Desinstalar",
            btn_cancel: "Cancelar",
            btn_close: "Fechar",
            status_ready: "Pronto para instalar em segundos.",
            status_ready_update: "Gugu Client já instalado. Escolha atualizar ou desinstalar.",
            status_stopping: "Fechando instâncias em execução...",
            status_extracting: "Extraindo GuguClient.exe...",
            status_shortcuts: "Criando atalhos...",
            status_registering: "Registrando no Windows...",
            status_done_install: "Instalação concluída!",
            status_removing: "Removendo arquivos e atalhos...",
            status_done_uninstall: "Desinstalação concluída.",
            status_error: "Erro na operação.",
        },
        Lang::Es => &Strings {
            window_title: "Gugu Client Setup",
            subtitle: "Instalador nativo en Rust",
            install_dir_label: "Carpeta de instalación:",
            cb_desktop: "Crear acceso directo en el Escritorio",
            cb_start_menu: "Crear acceso directo en el Menú Inicio",
            cb_launch: "Abrir Gugu Client al finalizar",
            cb_remove_data: "Eliminar cuentas guardadas, instancias y ajustes",
            uninstall_desc: "Esto eliminará Gugu Client y sus accesos directos de este equipo.",
            btn_install: "Instalar",
            btn_update: "Actualizar",
            btn_uninstall: "Desinstalar",
            btn_cancel: "Cancelar",
            btn_close: "Cerrar",
            status_ready: "Listo para instalar en segundos.",
            status_ready_update: "Gugu Client ya está instalado. Puedes actualizar o desinstalar.",
            status_stopping: "Cerrando procesos en ejecución...",
            status_extracting: "Extrayendo GuguClient.exe...",
            status_shortcuts: "Creando accesos directos...",
            status_registering: "Registrando en Windows...",
            status_done_install: "¡Instalación completada!",
            status_removing: "Eliminando archivos y accesos directos...",
            status_done_uninstall: "Desinstalación completada.",
            status_error: "Error en la operación.",
        },
        Lang::En => &Strings {
            window_title: "Gugu Client Setup",
            subtitle: "Fast native Rust installer",
            install_dir_label: "Install location:",
            cb_desktop: "Create Desktop shortcut",
            cb_start_menu: "Create Start Menu shortcut",
            cb_launch: "Launch Gugu Client after setup",
            cb_remove_data: "Remove saved accounts, instances, and settings",
            uninstall_desc: "This will remove Gugu Client and its shortcuts from this PC.",
            btn_install: "Install",
            btn_update: "Update",
            btn_uninstall: "Uninstall",
            btn_cancel: "Cancel",
            btn_close: "Close",
            status_ready: "Ready for a clean, fast install.",
            status_ready_update: "Gugu Client is already installed. Update or uninstall below.",
            status_stopping: "Closing running instances...",
            status_extracting: "Extracting GuguClient.exe...",
            status_shortcuts: "Creating shortcuts...",
            status_registering: "Registering with Windows...",
            status_done_install: "Installation complete!",
            status_removing: "Removing files and shortcuts...",
            status_done_uninstall: "Uninstall complete.",
            status_error: "Operation failed.",
        },
    }
}

fn detect_language() -> Lang {
    const LOCALE_NAME_MAX_LENGTH: usize = 85;
    let mut buf = [0u16; LOCALE_NAME_MAX_LENGTH];
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    if len > 1 {
        let locale = String::from_utf16_lossy(&buf[..(len as usize - 1)]).to_lowercase();
        if locale.starts_with("pt") {
            return Lang::PtBr;
        }
        if locale.starts_with("es") {
            return Lang::Es;
        }
    }
    Lang::En
}

fn to_wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let raw: PWSTR = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        s.map(PathBuf::from)
    }
}

fn install_dir() -> PathBuf {
    known_folder(&FOLDERID_LocalAppData)
        .unwrap_or_else(|| PathBuf::from(r"C:\Users\Public"))
        .join("GuguClient")
}

fn desktop_shortcut_path() -> Option<PathBuf> {
    Some(known_folder(&FOLDERID_Desktop)?.join("Gugu Client.lnk"))
}

fn start_menu_shortcut_path() -> Option<PathBuf> {
    Some(known_folder(&FOLDERID_Programs)?.join("Gugu Client.lnk"))
}

fn data_dir() -> Option<PathBuf> {
    Some(known_folder(&FOLDERID_Profile)?.join(".gugu-for-roblox"))
}

fn stop_running_gugu_client() {
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if name.eq_ignore_ascii_case("GuguClient.exe") {
                    if let Ok(proc) = OpenProcess(
                        PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                        false,
                        entry.th32ProcessID,
                    ) {
                        let _ = TerminateProcess(proc, 0);
                        let _ = WaitForSingleObject(proc, 3000);
                        let _ = CloseHandle(proc);
                    }
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

fn create_shortcut(target: &Path, shortcut: &Path, description: &str) -> Result<(), String> {
    if let Some(parent) = shortcut.parent() {
        let _ = fs::create_dir_all(parent);
    }
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let should_uninit = hr.is_ok();
        let res = (|| -> Result<(), String> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| e.to_string())?;
            let target_w = to_wide(&target.to_string_lossy());
            let dir_w = to_wide(
                &target
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            );
            let desc_w = to_wide(description);
            link.SetPath(PCWSTR(target_w.as_ptr()))
                .map_err(|e| e.to_string())?;
            link.SetWorkingDirectory(PCWSTR(dir_w.as_ptr()))
                .map_err(|e| e.to_string())?;
            link.SetDescription(PCWSTR(desc_w.as_ptr()))
                .map_err(|e| e.to_string())?;
            link.SetIconLocation(PCWSTR(target_w.as_ptr()), 0)
                .map_err(|e| e.to_string())?;

            let persist: IPersistFile = link.cast().map_err(|e| e.to_string())?;
            let shortcut_w = to_wide(&shortcut.to_string_lossy());
            persist
                .Save(PCWSTR(shortcut_w.as_ptr()), true)
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        if should_uninit {
            CoUninitialize();
        }
        res
    }
}

fn reg_set_sz(key: HKEY, name: &str, val: &str) {
    let name_w = to_wide(name);
    let val_w = to_wide(val);
    let bytes = unsafe {
        std::slice::from_raw_parts(val_w.as_ptr() as *const u8, val_w.len() * 2)
    };
    unsafe {
        let _ = RegSetValueExW(key, PCWSTR(name_w.as_ptr()), Some(0), REG_SZ, Some(bytes));
    }
}

fn reg_set_dword(key: HKEY, name: &str, val: u32) {
    let name_w = to_wide(name);
    let bytes = val.to_le_bytes();
    unsafe {
        let _ = RegSetValueExW(
            key,
            PCWSTR(name_w.as_ptr()),
            Some(0),
            REG_DWORD,
            Some(&bytes),
        );
    }
}

fn register_uninstall_entry(dir: &Path, client_exe: &Path, uninstaller: &Path, raw_size: usize) {
    unsafe {
        let subkey = to_wide(UNINSTALL_REG_SUBKEY);
        let mut hkey = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            Some(0),
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
        .is_ok()
        {
            reg_set_sz(hkey, "DisplayName", "Gugu Client for Roblox");
            reg_set_sz(hkey, "DisplayVersion", APP_VERSION);
            reg_set_sz(hkey, "Publisher", "guguloso");
            reg_set_sz(hkey, "InstallLocation", &dir.to_string_lossy());
            reg_set_sz(hkey, "DisplayIcon", &client_exe.to_string_lossy());
            reg_set_sz(
                hkey,
                "UninstallString",
                &format!("\"{}\" --uninstall", uninstaller.display()),
            );
            reg_set_sz(
                hkey,
                "QuietUninstallString",
                &format!("\"{}\" --uninstall --silent", uninstaller.display()),
            );
            reg_set_sz(
                hkey,
                "URLInfoAbout",
                "https://github.com/guguloso/guguclient",
            );
            reg_set_dword(hkey, "NoModify", 1);
            reg_set_dword(hkey, "NoRepair", 1);
            let kb = ((raw_size / 1024) as u32).max(23_000);
            reg_set_dword(hkey, "EstimatedSize", kb);
            let _ = RegCloseKey(hkey);
        }
    }
}

fn remove_uninstall_entry() {
    unsafe {
        let subkey = to_wide(UNINSTALL_REG_SUBKEY);
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr()));
    }
}

fn perform_install<F: Fn(i32, &str)>(
    desktop: bool,
    start_menu: bool,
    launch_after: bool,
    s: &Strings,
    report: F,
) -> Result<(), String> {
    if PAYLOAD_DEFLATE.is_empty() {
        return Err("Embedded GuguClient.exe payload is missing.".into());
    }

    report(15, s.status_stopping);
    stop_running_gugu_client();

    let dir = install_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create {}: {e}", dir.display()))?;
    let client_exe = dir.join("GuguClient.exe");
    let uninstaller = dir.join("Uninstall.exe");

    report(45, s.status_extracting);
    let mut decoder = DeflateDecoder::new(PAYLOAD_DEFLATE);
    let mut bytes = Vec::with_capacity(24 * 1024 * 1024);
    decoder
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Decompression failed: {e}"))?;
    {
        let mut f = File::create(&client_exe)
            .map_err(|e| format!("Failed to write {}: {e}", client_exe.display()))?;
        f.write_all(&bytes)
            .map_err(|e| format!("Write failed: {e}"))?;
    }

    if let Ok(self_exe) = std::env::current_exe() {
        if self_exe != uninstaller {
            let _ = fs::copy(&self_exe, &uninstaller);
        }
    }

    report(75, s.status_shortcuts);
    if desktop {
        if let Some(p) = desktop_shortcut_path() {
            create_shortcut(&client_exe, &p, "Gugu Client for Roblox")?;
        }
    }
    if start_menu {
        if let Some(p) = start_menu_shortcut_path() {
            create_shortcut(&client_exe, &p, "Gugu Client for Roblox")?;
        }
    }

    report(90, s.status_registering);
    register_uninstall_entry(&dir, &client_exe, &uninstaller, bytes.len());

    report(100, s.status_done_install);
    if launch_after {
        let _ = Command::new(&client_exe).spawn();
    }
    Ok(())
}

fn perform_uninstall<F: Fn(i32, &str)>(
    remove_user_data: bool,
    s: &Strings,
    report: F,
) -> Result<(), String> {
    report(20, s.status_stopping);
    stop_running_gugu_client();

    report(55, s.status_removing);
    let dir = install_dir();
    let client_exe = dir.join("GuguClient.exe");
    let _ = fs::remove_file(&client_exe);
    if let Some(p) = desktop_shortcut_path() {
        let _ = fs::remove_file(p);
    }
    if let Some(p) = start_menu_shortcut_path() {
        let _ = fs::remove_file(p);
    }
    remove_uninstall_entry();

    if remove_user_data {
        if let Some(d) = data_dir() {
            let _ = fs::remove_dir_all(d);
        }
    }

    report(100, s.status_done_uninstall);
    let _ = Command::new("cmd.exe")
        .args([
            "/C",
            &format!(
                "ping 127.0.0.1 -n 2 >nul & rmdir /S /Q \"{}\"",
                dir.display()
            ),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
    Ok(())
}

// ------------------------------------------------------------------ GUI

#[derive(Clone, Copy, PartialEq, Eq)]
enum HitTarget {
    None,
    Cb1,
    Cb2,
    Cb3,
    PrimaryBtn,
    SecondaryBtn,
    UninstallBtn,
}

struct AppState {
    s: &'static Strings,
    uninstall_mode: bool,
    already_installed: bool,
    cb_desktop: bool,
    cb_start_menu: bool,
    cb_launch: bool,
    cb_remove_data: bool,
    busy: bool,
    finished: bool,
    launch_scheduled: bool,
    progress: i32,
    status: String,
    hover: HitTarget,
    icon: HICON,
    font_title: HFONT,
    font_sub: HFONT,
    font_body: HFONT,
    font_small: HFONT,
    dir_display: String,
    worker_error: Arc<Mutex<Option<String>>>,
    closing: AtomicBool,
}

impl AppState {
    fn new(uninstall_mode: bool) -> Self {
        let lang = detect_language();
        let s = strings(lang);
        let dir = install_dir();
        let already_installed = dir.join("GuguClient.exe").is_file();
        let status = if uninstall_mode {
            s.uninstall_desc.to_owned()
        } else if already_installed {
            s.status_ready_update.to_owned()
        } else {
            s.status_ready.to_owned()
        };
        Self {
            s,
            uninstall_mode,
            already_installed,
            cb_desktop: true,
            cb_start_menu: true,
            cb_launch: true,
            cb_remove_data: false,
            busy: false,
            finished: false,
            launch_scheduled: false,
            progress: 0,
            status,
            hover: HitTarget::None,
            icon: HICON::default(),
            font_title: HFONT::default(),
            font_sub: HFONT::default(),
            font_body: HFONT::default(),
            font_small: HFONT::default(),
            dir_display: dir.to_string_lossy().into_owned(),
            worker_error: Arc::new(Mutex::new(None)),
            closing: AtomicBool::new(false),
        }
    }
}

fn make_font(height: i32, weight: i32) -> HFONT {
    let face = to_wide("Segoe UI");
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
            PCWSTR(face.as_ptr()),
        )
    }
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

fn in_rect(pt: POINT, r: RECT) -> bool {
    pt.x >= r.left && pt.x <= r.right && pt.y >= r.top && pt.y <= r.bottom
}

struct Layout {
    card: RECT,
    cb1: RECT,
    cb2: RECT,
    cb3: RECT,
    progress_bar: RECT,
    primary_btn: RECT,
    secondary_btn: RECT,
    uninstall_btn: RECT,
}

fn layout_for(st: &AppState) -> Layout {
    let card = rect(18, 84, WIN_W - 18, 250);
    let cb1 = rect(34, 126, WIN_W - 34, 152);
    let cb2 = rect(34, 156, WIN_W - 34, 182);
    let cb3 = rect(34, 186, WIN_W - 34, 212);
    let progress_bar = rect(34, 224, WIN_W - 34, 232);
    let primary_btn = rect(WIN_W - 18 - 116, 274, WIN_W - 18, 312);
    let secondary_btn = rect(WIN_W - 18 - 116 - 10 - 104, 274, WIN_W - 18 - 116 - 10, 312);
    let uninstall_btn = if !st.uninstall_mode && st.already_installed && !st.busy && !st.finished {
        rect(18, 274, 124, 312)
    } else {
        rect(0, 0, 0, 0)
    };
    Layout {
        card,
        cb1,
        cb2,
        cb3,
        progress_bar,
        primary_btn,
        secondary_btn,
        uninstall_btn,
    }
}

fn hit_test(st: &AppState, pt: POINT) -> HitTarget {
    let l = layout_for(st);
    if !st.busy && !st.finished {
        if st.uninstall_mode {
            if in_rect(pt, l.cb1) {
                return HitTarget::Cb1;
            }
        } else {
            if in_rect(pt, l.cb1) {
                return HitTarget::Cb1;
            }
            if in_rect(pt, l.cb2) {
                return HitTarget::Cb2;
            }
            if in_rect(pt, l.cb3) {
                return HitTarget::Cb3;
            }
        }
        if in_rect(pt, l.uninstall_btn) {
            return HitTarget::UninstallBtn;
        }
        if in_rect(pt, l.secondary_btn) {
            return HitTarget::SecondaryBtn;
        }
    }
    if !st.busy && in_rect(pt, l.primary_btn) {
        return HitTarget::PrimaryBtn;
    }
    HitTarget::None
}

fn rgb(r: u8, g: u8, b: u8) -> windows::Win32::Foundation::COLORREF {
    windows::Win32::Foundation::COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

unsafe fn draw_text(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    text: &str,
    mut r: RECT,
    font: HFONT,
    color: windows::Win32::Foundation::COLORREF,
    flags: u32,
) {
    SelectObject(hdc, font.into());
    SetTextColor(hdc, color);
    let mut w = to_wide(text);
    // Remove null terminator for length parameter
    let len = w.len().saturating_sub(1) as i32;
    DrawTextW(
        hdc,
        &mut w[..len as usize],
        &mut r,
        windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT(flags),
    );
}

unsafe fn draw_rounded_box(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    r: RECT,
    radius: i32,
    fill: windows::Win32::Foundation::COLORREF,
    border: windows::Win32::Foundation::COLORREF,
) {
    let brush = CreateSolidBrush(fill);
    let pen = CreatePen(PS_SOLID, 1, border);
    let old_b = SelectObject(hdc, brush.into());
    let old_p = SelectObject(hdc, pen.into());
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
    SelectObject(hdc, old_b);
    SelectObject(hdc, old_p);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());
}

unsafe fn draw_checkbox(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    r: RECT,
    checked: bool,
    hovered: bool,
    label: &str,
    font: HFONT,
) {
    let box_rect = rect(r.left, r.top + 4, r.left + 18, r.top + 22);
    let fill = if checked {
        rgb(244, 244, 245)
    } else if hovered {
        rgb(39, 39, 46)
    } else {
        rgb(24, 24, 29)
    };
    let border = if checked {
        rgb(244, 244, 245)
    } else if hovered {
        rgb(113, 113, 122)
    } else {
        rgb(63, 63, 70)
    };
    draw_rounded_box(hdc, box_rect, 5, fill, border);

    if checked {
        let pen = CreatePen(PS_SOLID, 2, rgb(12, 12, 15));
        let old_p = SelectObject(hdc, pen.into());
        let _ = MoveToEx(hdc, box_rect.left + 4, box_rect.top + 9, None);
        let _ = LineTo(hdc, box_rect.left + 7, box_rect.top + 13);
        let _ = LineTo(hdc, box_rect.left + 14, box_rect.top + 5);
        SelectObject(hdc, old_p);
        let _ = DeleteObject(pen.into());
    }

    let text_rect = rect(r.left + 28, r.top, r.right, r.bottom);
    draw_text(
        hdc,
        label,
        text_rect,
        font,
        rgb(228, 228, 231),
        DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0 | DT_END_ELLIPSIS.0,
    );
}

unsafe fn paint(hwnd: HWND, st: &AppState) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc_win = BeginPaint(hwnd, &mut ps);
    let mut client: RECT = zeroed();
    let _ = GetClientRect(hwnd, &mut client);
    let w = client.right - client.left;
    let h = client.bottom - client.top;

    let hdc = CreateCompatibleDC(Some(hdc_win));
    let bmp = CreateCompatibleBitmap(hdc_win, w, h);
    let old_bmp = SelectObject(hdc, bmp.into());
    SetBkMode(hdc, TRANSPARENT);

    // Window background (#0B0B0E)
    let bg_brush = CreateSolidBrush(rgb(11, 11, 14));
    FillRect(hdc, &client, bg_brush);
    let _ = DeleteObject(bg_brush.into());

    // Header icon + title + subtitle
    if !st.icon.is_invalid() {
        let _ = DrawIconEx(hdc, 20, 20, st.icon, 44, 44, 0, None, DI_NORMAL);
    }
    draw_text(
        hdc,
        "Gugu Client for Roblox",
        rect(76, 18, WIN_W - 18, 44),
        st.font_title,
        rgb(250, 250, 250),
        DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
    );
    let sub = format!("v{}  •  {}", APP_VERSION, st.s.subtitle);
    draw_text(
        hdc,
        &sub,
        rect(76, 44, WIN_W - 18, 64),
        st.font_sub,
        rgb(161, 161, 170),
        DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
    );

    let l = layout_for(st);

    // Card (#141418)
    draw_rounded_box(hdc, l.card, 12, rgb(20, 20, 25), rgb(39, 39, 46));

    // Install location header inside card
    if st.uninstall_mode {
        draw_text(
            hdc,
            st.s.uninstall_desc,
            rect(34, 96, WIN_W - 34, 120),
            st.font_sub,
            rgb(212, 212, 216),
            DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0 | DT_END_ELLIPSIS.0,
        );
        draw_checkbox(
            hdc,
            l.cb1,
            st.cb_remove_data,
            st.hover == HitTarget::Cb1,
            st.s.cb_remove_data,
            st.font_body,
        );
    } else {
        let path_line = format!("{} {}", st.s.install_dir_label, st.dir_display);
        draw_text(
            hdc,
            &path_line,
            rect(34, 96, WIN_W - 34, 118),
            st.font_small,
            rgb(138, 138, 148),
            DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0 | DT_END_ELLIPSIS.0,
        );
        draw_checkbox(
            hdc,
            l.cb1,
            st.cb_desktop,
            st.hover == HitTarget::Cb1,
            st.s.cb_desktop,
            st.font_body,
        );
        draw_checkbox(
            hdc,
            l.cb2,
            st.cb_start_menu,
            st.hover == HitTarget::Cb2,
            st.s.cb_start_menu,
            st.font_body,
        );
        draw_checkbox(
            hdc,
            l.cb3,
            st.cb_launch,
            st.hover == HitTarget::Cb3,
            st.s.cb_launch,
            st.font_body,
        );
    }

    // Progress bar + status
    draw_rounded_box(hdc, l.progress_bar, 6, rgb(32, 32, 38), rgb(44, 44, 52));
    if st.progress > 0 {
        let fill_w = ((l.progress_bar.right - l.progress_bar.left) * st.progress.clamp(0, 100)) / 100;
        if fill_w > 2 {
            let fill_r = rect(
                l.progress_bar.left,
                l.progress_bar.top,
                l.progress_bar.left + fill_w,
                l.progress_bar.bottom,
            );
            draw_rounded_box(hdc, fill_r, 6, rgb(34, 197, 94), rgb(34, 197, 94));
        }
    }

    // Status message above bottom buttons
    draw_text(
        hdc,
        &st.status,
        rect(20, 252, WIN_W - 20, 270),
        st.font_small,
        rgb(161, 161, 170),
        DT_LEFT.0 | DT_VCENTER.0 | DT_SINGLELINE.0 | DT_END_ELLIPSIS.0,
    );

    // Uninstall button (left, if already installed)
    if l.uninstall_btn.right > 0 {
        let fill = if st.hover == HitTarget::UninstallBtn {
            rgb(56, 26, 28)
        } else {
            rgb(34, 20, 22)
        };
        let border = rgb(127, 29, 29);
        draw_rounded_box(hdc, l.uninstall_btn, 10, fill, border);
        draw_text(
            hdc,
            st.s.btn_uninstall,
            l.uninstall_btn,
            st.font_body,
            rgb(252, 165, 165),
            DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
        );
    }

    // Secondary Cancel button
    if !st.busy && !st.finished {
        let fill = if st.hover == HitTarget::SecondaryBtn {
            rgb(39, 39, 46)
        } else {
            rgb(24, 24, 29)
        };
        draw_rounded_box(hdc, l.secondary_btn, 10, fill, rgb(63, 63, 70));
        draw_text(
            hdc,
            st.s.btn_cancel,
            l.secondary_btn,
            st.font_body,
            rgb(228, 228, 231),
            DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
        );
    }

    // Primary button
    let primary_label = if st.finished {
        st.s.btn_close
    } else if st.uninstall_mode {
        st.s.btn_uninstall
    } else if st.already_installed {
        st.s.btn_update
    } else {
        st.s.btn_install
    };
    let (p_fill, p_text) = if st.busy {
        (rgb(63, 63, 70), rgb(161, 161, 170))
    } else if st.uninstall_mode && !st.finished {
        if st.hover == HitTarget::PrimaryBtn {
            (rgb(239, 68, 68), rgb(255, 255, 255))
        } else {
            (rgb(220, 38, 38), rgb(255, 255, 255))
        }
    } else if st.hover == HitTarget::PrimaryBtn {
        (rgb(228, 228, 231), rgb(9, 9, 11))
    } else {
        (rgb(250, 250, 250), rgb(9, 9, 11))
    };
    draw_rounded_box(hdc, l.primary_btn, 10, p_fill, p_fill);
    draw_text(
        hdc,
        primary_label,
        l.primary_btn,
        st.font_title,
        p_text,
        DT_CENTER.0 | DT_VCENTER.0 | DT_SINGLELINE.0,
    );

    let _ = BitBlt(hdc_win, 0, 0, w, h, Some(hdc), 0, 0, SRCCOPY);
    SelectObject(hdc, old_bmp);
    let _ = DeleteObject(bmp.into());
    let _ = DeleteDC(hdc);
    let _ = EndPaint(hwnd, &ps);
}

static mut STATE_PTR: *mut AppState = std::ptr::null_mut();

unsafe fn start_worker(hwnd: HWND, st: &mut AppState, uninstall: bool) {
    st.busy = true;
    st.progress = 5;
    st.uninstall_mode = uninstall;
    *st.worker_error.lock().unwrap() = None;
    let _ = InvalidateRect(Some(hwnd), None, false);

    let hwnd_val = hwnd.0 as isize;
    let desktop = st.cb_desktop;
    let start_menu = st.cb_start_menu;
    let launch_after = st.cb_launch;
    let remove_data = st.cb_remove_data;
    let s = st.s;
    let err_slot = Arc::clone(&st.worker_error);

    st.launch_scheduled = !uninstall && launch_after;

    thread::spawn(move || {
        let report = |pct: i32, msg: &str| {
            let boxed = Box::into_raw(Box::new(msg.to_owned()));
            let _ = PostMessageW(
                Some(HWND(hwnd_val as *mut _)),
                WM_APP_PROGRESS,
                WPARAM(pct as usize),
                LPARAM(boxed as isize),
            );
        };
        let res = if uninstall {
            perform_uninstall(remove_data, s, report)
        } else {
            perform_install(desktop, start_menu, launch_after, s, report)
        };
        if let Err(err) = res {
            *err_slot.lock().unwrap() = Some(err);
            let _ = PostMessageW(Some(HWND(hwnd_val as *mut _)), WM_APP_DONE, WPARAM(0), LPARAM(0));
        } else {
            let _ = PostMessageW(Some(HWND(hwnd_val as *mut _)), WM_APP_DONE, WPARAM(1), LPARAM(0));
        }
    });
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let st = if STATE_PTR.is_null() {
        None
    } else {
        Some(&mut *STATE_PTR)
    };

    match msg {
        WM_CREATE => {
            let dark: i32 = 1;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &dark as *const _ as *const _,
                size_of::<i32>() as u32,
            );
            let corner = DWMWCP_ROUND.0;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const _ as *const _,
                size_of::<i32>() as u32,
            );
            LRESULT(0)
        }
        WM_PAINT => {
            if let Some(st) = st {
                paint(hwnd, st);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(st) = st {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let next = hit_test(st, POINT { x, y });
                if next != st.hover {
                    st.hover = next;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(st) = st {
                if st.hover != HitTarget::None {
                    if let Ok(cur) = LoadCursorW(None, IDC_HAND) {
                        windows::Win32::UI::WindowsAndMessaging::SetCursor(Some(cur));
                        return LRESULT(1);
                    }
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            if let Some(st) = st {
                let x = (lparam.0 & 0xFFFF) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                match hit_test(st, POINT { x, y }) {
                    HitTarget::Cb1 if st.uninstall_mode => {
                        st.cb_remove_data = !st.cb_remove_data;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    HitTarget::Cb1 => {
                        st.cb_desktop = !st.cb_desktop;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    HitTarget::Cb2 => {
                        st.cb_start_menu = !st.cb_start_menu;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    HitTarget::Cb3 => {
                        st.cb_launch = !st.cb_launch;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                    HitTarget::SecondaryBtn => {
                        let _ = DestroyWindow(hwnd);
                    }
                    HitTarget::UninstallBtn => {
                        start_worker(hwnd, st, true);
                    }
                    HitTarget::PrimaryBtn => {
                        if st.finished {
                            let _ = DestroyWindow(hwnd);
                        } else if !st.busy {
                            start_worker(hwnd, st, st.uninstall_mode);
                        }
                    }
                    HitTarget::None => {}
                }
            }
            LRESULT(0)
        }
        WM_APP_PROGRESS => {
            let text = Box::from_raw(lparam.0 as *mut String);
            if let Some(st) = st {
                st.progress = wparam.0 as i32;
                st.status = *text;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_APP_DONE => {
            if let Some(st) = st {
                st.busy = false;
                if wparam.0 == 1 {
                    st.finished = true;
                    st.progress = 100;
                    if st.launch_scheduled {
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                } else if let Some(err) = st.worker_error.lock().unwrap().take() {
                    st.progress = 0;
                    st.status = format!("{} {}", st.s.status_error, err);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            if let Some(st) = st {
                if st.busy {
                    return LRESULT(0);
                }
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            if let Some(st) = st {
                st.closing.store(true, Ordering::Relaxed);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let exe_is_uninstaller = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
        .is_some_and(|n| n.contains("uninstall"));
    let uninstall_mode = exe_is_uninstaller || args.iter().any(|a| a == "--uninstall");
    let silent = args.iter().any(|a| a == "--silent");

    let lang = detect_language();
    let s = strings(lang);

    if silent {
        let res = if uninstall_mode {
            perform_uninstall(false, s, |_, _| {})
        } else {
            perform_install(true, true, false, s, |_, _| {})
        };
        if res.is_err() {
            std::process::exit(1);
        }
        return;
    }

    let mut st = Box::new(AppState::new(uninstall_mode));
    unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        st.icon = LoadIconW(Some(hinst.into()), PCWSTR(1 as *const u16)).unwrap_or_default();
        st.font_title = make_font(-19, FW_BOLD.0 as i32);
        st.font_sub = make_font(-13, FW_NORMAL.0 as i32);
        st.font_body = make_font(-14, FW_SEMIBOLD.0 as i32);
        st.font_small = make_font(-12, FW_NORMAL.0 as i32);

        STATE_PTR = &mut *st;

        let class_name = to_wide("GuguSetupWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: st.icon,
            ..zeroed()
        };
        RegisterClassW(&wc);

        // Adjust outer window size for standard caption/frame
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT {
            left: 0,
            top: 0,
            right: WIN_W,
            bottom: WIN_H,
        };
        let _ = windows::Win32::UI::WindowsAndMessaging::AdjustWindowRect(&mut r, style, false);
        let full_w = r.right - r.left;
        let full_h = r.bottom - r.top;
        let cx = (GetSystemMetrics(SM_CXSCREEN) - full_w) / 2;
        let cy = (GetSystemMetrics(SM_CYSCREEN) - full_h) / 2;

        let title_w = to_wide(st.s.window_title);
        let hwnd = CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            style,
            cx.max(0),
            cy.max(0),
            full_w,
            full_h,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("create setup window");

        let _ = SetWindowPos(hwnd, None, cx.max(0), cy.max(0), full_w, full_h, SWP_NOZORDER);
        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let hdc = GetDC(None);
        let _ = ReleaseDC(None, hdc);
        let _ = DeleteObject(st.font_title.into());
        let _ = DeleteObject(st.font_sub.into());
        let _ = DeleteObject(st.font_body.into());
        let _ = DeleteObject(st.font_small.into());
        STATE_PTR = std::ptr::null_mut();
    }
}
