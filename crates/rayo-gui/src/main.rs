#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use arboard::Clipboard;
use clap::Parser;
use rayo_core::{ContentSearchOptions, FileIndex, SearchOptions, load_index, search_content};
use serde::{Deserialize, Serialize};
use slint::{ModelRc, Timer, TimerMode, VecModel};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, RegisterHotKey, UnregisterHotKey, VK_SPACE,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetMessageW, SW_RESTORE, SW_SHOWNORMAL, SetForegroundWindow,
    ShowWindow, TranslateMessage, WM_HOTKEY,
};
use windows::core::PCWSTR;
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

slint::include_modules!();

const DEFAULT_PIPE: &str = r"\\.\pipe\rayo-query";
const RAYO_GUI_MUTEX_NAME: &str = "Global\\RayoGuiSingleton";
const RAYO_HOTKEY_ID: i32 = 0x5261;
const RELEASES_LATEST_API: &str = "https://api.github.com/repos/waar19/rayo/releases/latest";
const APP_CATALOG_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Parser, Debug)]
#[command(author, version, about = "Rayo GUI search client")]
struct Cli {
    #[arg(long, default_value = "index.rayo")]
    index: PathBuf,
    #[arg(long)]
    under: Option<String>,
    #[arg(long)]
    query: Option<String>,
    #[arg(long)]
    open: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_PIPE)]
    pipe: String,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    debounce_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryRequest {
    query: String,
    extension: Option<String>,
    under_dir: Option<String>,
    glob: Option<String>,
    mode: Option<String>,
    timeout_ms: Option<u64>,
    directories_only: bool,
    files_only: bool,
    fuzzy: bool,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryResultDto {
    path: String,
    is_directory: bool,
    #[serde(default)]
    is_app: bool,
    line_number: Option<u64>,
    line_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryResponse {
    took_ms: u128,
    total_entries: usize,
    results: Vec<QueryResultDto>,
    status: Option<String>,
    indexed_entries: Option<usize>,
    metrics: Option<QueryMetricsDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryMetricsDto {
    requests_total: u64,
    avg_took_ms: f64,
    last_took_ms: u128,
    max_took_ms: u128,
    indexed_entries: usize,
}

struct PipeClient {
    stream: File,
    reader: BufReader<File>,
}

impl PipeClient {
    fn connect(pipe_name: &str) -> Result<Self> {
        let stream = OpenOptions::new()
            .read(true)
            .write(true)
            .open(pipe_name)
            .with_context(|| format!("failed to connect service pipe {pipe_name}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .context("failed to clone stream for pipe reader")?,
        );
        Ok(Self { stream, reader })
    }

    fn query(&mut self, request: &QueryRequest) -> Result<QueryResponse> {
        serde_json::to_writer(&mut self.stream, request).context("failed to serialize request")?;
        self.stream
            .write_all(b"\n")
            .context("failed to write request terminator")?;
        self.stream.flush().context("failed to flush request")?;

        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .context("failed to read response line")?;
        if read == 0 {
            return Err(anyhow!("service closed pipe without response"));
        }
        let response: QueryResponse =
            serde_json::from_str(line.trim_end()).context("invalid response JSON")?;
        Ok(response)
    }
}

#[derive(Clone)]
struct GuiConfig {
    index_path: PathBuf,
    pipe_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct GuiSettings {
    under_dir: Option<String>,
    extension: Option<String>,
    files_only: bool,
    directories_only: bool,
    limit: usize,
    debounce_ms: u64,
    content_mode: bool,
    fuzzy_mode: bool,
    apps_mode: bool,
    theme_auto: bool,
    light_theme: bool,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            under_dir: None,
            extension: None,
            files_only: false,
            directories_only: false,
            limit: 200,
            debounce_ms: 10,
            content_mode: false,
            fuzzy_mode: false,
            apps_mode: true,
            theme_auto: true,
            light_theme: false,
        }
    }
}

#[derive(Debug, Clone)]
struct AppEntry {
    display_name: String,
    launch_path: String,
}

#[derive(Default)]
struct AppCatalogCache {
    built_at: Option<Instant>,
    entries: Vec<AppEntry>,
}

struct UiPayload {
    rows: Vec<UiRowData>,
    paths: Vec<String>,
    status_text: String,
    mode_text: String,
}

#[derive(Clone)]
struct UiRowData {
    kind: String,
    name: String,
    path: String,
    subtitle: String,
    is_directory: bool,
    is_app: bool,
    line_match: bool,
}

#[derive(Debug)]
struct SearchCommand {
    request_id: u64,
    query: String,
}

fn normalize_optional_text(input: String) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn file_name_query(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|value| value.to_string())
}

fn apply_open_context(
    open_path: &Path,
    under_dir: &mut Option<String>,
    query: &mut Option<String>,
) {
    if open_path.is_dir() {
        *under_dir = Some(open_path.display().to_string());
        return;
    }

    if let Some(parent) = open_path.parent() {
        *under_dir = Some(parent.display().to_string());
    }

    if query.is_none() {
        *query = file_name_query(open_path);
    }
}

fn sanitize_settings(settings: &mut GuiSettings) {
    settings.under_dir = settings
        .under_dir
        .as_ref()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    settings.extension = settings
        .extension
        .as_ref()
        .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    settings.limit = settings.limit.clamp(10, 1000);
    settings.debounce_ms = settings.debounce_ms.clamp(0, 500);
    if settings.files_only && settings.directories_only {
        settings.directories_only = false;
    }
}

fn settings_hint(settings: &GuiSettings) -> String {
    let scope = settings.under_dir.as_deref().unwrap_or("all");
    let extension = settings.extension.as_deref().unwrap_or("*");
    let mode = if settings.files_only {
        "files"
    } else if settings.directories_only {
        "folders"
    } else {
        "all"
    };
    let search_mode = if settings.content_mode {
        "content"
    } else {
        "name"
    };
    let fuzzy = if settings.fuzzy_mode { "on" } else { "off" };
    let apps = if settings.apps_mode { "on" } else { "off" };
    let theme = if settings.theme_auto {
        "auto"
    } else if settings.light_theme {
        "light"
    } else {
        "dark"
    };
    format!(
        "search={search_mode} scope={scope} ext={extension} mode={mode} fuzzy={fuzzy} apps={apps} theme={theme} limit={} debounce={}ms",
        settings.limit, settings.debounce_ms
    )
}

fn validate_settings(settings: &GuiSettings) -> Option<String> {
    if let Some(extension) = settings.extension.as_ref() {
        if !extension
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return Some("Extension filter only allows letters, numbers, '_' and '-'.".to_string());
        }
    }
    if settings.files_only && settings.directories_only {
        return Some("Choose either Files only or Folders only.".to_string());
    }
    None
}

fn settings_path() -> Result<PathBuf> {
    let app_data = std::env::var("APPDATA").context("APPDATA environment variable is not set")?;
    Ok(PathBuf::from(app_data).join("rayo").join("settings.json"))
}

fn detect_system_light_theme() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key =
        match hkcu.open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize") {
            Ok(key) => key,
            Err(_) => return false,
        };
    key.get_value::<u32, _>("AppsUseLightTheme")
        .map(|value| value != 0)
        .unwrap_or(false)
}

fn load_settings() -> Result<(GuiSettings, Option<String>)> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok((GuiSettings::default(), None));
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let mut settings: GuiSettings = match serde_json::from_str(&raw) {
        Ok(settings) => settings,
        Err(_) => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let backup = path.with_extension(format!("corrupt-{stamp}.json"));
            let _ = fs::rename(&path, &backup);
            return Ok((
                GuiSettings::default(),
                Some(format!(
                    "Invalid settings were reset. Backup: {}",
                    backup.display()
                )),
            ));
        }
    };
    sanitize_settings(&mut settings);
    Ok((settings, None))
}

fn save_settings(settings: &GuiSettings) -> Result<()> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create settings directory {}", parent.display()))?;
    }
    let content =
        serde_json::to_string_pretty(settings).context("failed to serialize settings JSON")?;
    fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct UpdateCheckState {
    checked_at_epoch_secs: u64,
    latest_version: Option<String>,
}

impl Default for UpdateCheckState {
    fn default() -> Self {
        Self {
            checked_at_epoch_secs: 0,
            latest_version: None,
        }
    }
}

fn update_state_path() -> Result<PathBuf> {
    let app_data = std::env::var("APPDATA").context("APPDATA environment variable is not set")?;
    Ok(PathBuf::from(app_data)
        .join("rayo")
        .join("update-check.json"))
}

fn parse_semver(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

fn version_is_newer(latest: &str, current: &str) -> bool {
    let left = parse_semver(latest);
    let right = parse_semver(current);
    let max_len = left.len().max(right.len());
    for idx in 0..max_len {
        let a = *left.get(idx).unwrap_or(&0);
        let b = *right.get(idx).unwrap_or(&0);
        if a > b {
            return true;
        }
        if a < b {
            return false;
        }
    }
    false
}

fn maybe_check_updates() -> Option<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = update_state_path().ok()?;
    let state = if path.exists() {
        fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<UpdateCheckState>(&raw).ok())
            .unwrap_or_default()
    } else {
        UpdateCheckState::default()
    };

    if state.checked_at_epoch_secs > now.saturating_sub(24 * 60 * 60) {
        if let Some(latest) = state.latest_version
            && version_is_newer(&latest, env!("CARGO_PKG_VERSION"))
        {
            return Some(format!("Update available: {latest}. Run installer script."));
        }
        return None;
    }

    let response = ureq::get(RELEASES_LATEST_API)
        .set("User-Agent", "rayo-gui")
        .call()
        .ok()?;
    let payload: serde_json::Value = response.into_json().ok()?;
    let latest = payload
        .get("tag_name")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().trim_start_matches(['v', 'V']).to_string())
        .filter(|value| !value.is_empty());
    let next = UpdateCheckState {
        checked_at_epoch_secs: now,
        latest_version: latest.clone(),
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(serialized) = serde_json::to_string_pretty(&next) {
        let _ = fs::write(&path, serialized);
    }

    if let Some(latest) = latest
        && version_is_newer(&latest, env!("CARGO_PKG_VERSION"))
    {
        return Some(format!("Update available: {latest}. Run installer script."));
    }
    None
}

fn read_settings(settings: &Arc<RwLock<GuiSettings>>) -> GuiSettings {
    match settings.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

thread_local! {
    static UI_ICON_CACHE: RefCell<HashMap<String, slint::Image>> = RefCell::new(HashMap::new());
}

fn apply_payload(ui: &MainWindow, payload: UiPayload, paths: &Arc<Mutex<Vec<String>>>) {
    let rows = payload
        .rows
        .into_iter()
        .map(|row| ResultRow {
            icon: resolve_ui_row_icon(&row),
            kind: row.kind.into(),
            name: row.name.into(),
            path: row.path.into(),
            subtitle: row.subtitle.into(),
        })
        .collect::<Vec<_>>();
    let model = Rc::new(VecModel::from(rows));
    ui.set_results(ModelRc::from(model));
    ui.set_status_text(payload.status_text.into());
    ui.set_mode_text(payload.mode_text.into());
    ui.set_selected_index(-1);
    ui.set_show_context_menu(false);
    ui.set_context_row_index(-1);
    clear_preview(ui);
    if let Ok(mut guard) = paths.lock() {
        *guard = payload.paths;
    }
}

fn query_service(
    pipe_name: &str,
    request: &QueryRequest,
    pipe_client: &mut Option<PipeClient>,
) -> Result<QueryResponse> {
    if pipe_client.is_none() {
        *pipe_client = Some(PipeClient::connect(pipe_name)?);
    }

    if let Some(client) = pipe_client.as_mut() {
        match client.query(request) {
            Ok(response) => return Ok(response),
            Err(_) => {
                *pipe_client = None;
            }
        }
    }

    *pipe_client = Some(PipeClient::connect(pipe_name)?);
    let Some(client) = pipe_client.as_mut() else {
        return Err(anyhow!("failed to initialize named pipe client"));
    };
    client.query(request)
}

fn can_connect_pipe(pipe_name: &str) -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name)
        .is_ok()
}

fn ensure_fallback_index_loaded(
    config: &GuiConfig,
    fallback_index: &Arc<RwLock<Option<FileIndex>>>,
) -> Result<usize> {
    {
        let guard = match fallback_index.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(index) = guard.as_ref() {
            return Ok(index.entries.len());
        }
    }

    let mut guard = match fallback_index.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        let index = load_index(&config.index_path).with_context(|| {
            format!(
                "failed to load fallback index {}",
                config.index_path.display()
            )
        })?;
        *guard = Some(index);
    }
    Ok(guard.as_ref().map(|index| index.entries.len()).unwrap_or(0))
}

fn shell_open(path: &str, as_admin: bool) -> Result<()> {
    let verb = if as_admin { "runas" } else { "open" };
    let verb_w = to_utf16_null(verb);
    let path_w = to_utf16_null(path);

    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(path_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };

    if result.0 as isize <= 32 {
        return Err(anyhow!("ShellExecuteW failed with code {:?}", result.0));
    }
    Ok(())
}

fn open_containing_folder(path: &str) -> Result<()> {
    Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .spawn()
        .context("failed to launch explorer")?;
    Ok(())
}

fn to_utf16_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn acquire_single_instance() -> Result<Option<HANDLE>> {
    let name = to_utf16_null(RAYO_GUI_MUTEX_NAME);
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }
        .context("failed to create single-instance mutex")?;
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        return Ok(None);
    }
    Ok(Some(handle))
}

fn focus_existing_rayo_window() {
    let title = to_utf16_null("Rayo");
    let hwnd = match unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) } {
        Ok(hwnd) => hwnd,
        Err(_) => return,
    };
    if hwnd.0.is_null() {
        return;
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetForegroundWindow(hwnd);
    }
}

fn spawn_global_hotkey_listener() {
    thread::spawn(move || {
        let modifiers: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_ALT;
        if unsafe {
            RegisterHotKey(
                HWND::default(),
                RAYO_HOTKEY_ID,
                modifiers,
                VK_SPACE.0 as u32,
            )
        }
        .is_err()
        {
            return;
        }

        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        loop {
            let state = unsafe { GetMessageW(&mut message, HWND::default(), 0, 0) };
            if state.0 <= 0 {
                break;
            }

            if message.message == WM_HOTKEY && message.wParam == WPARAM(RAYO_HOTKEY_ID as usize) {
                focus_existing_rayo_window();
                continue;
            }

            unsafe {
                let _ = TranslateMessage(&message);
                let _ = DispatchMessageW(&message);
            }
        }

        unsafe {
            let _ = UnregisterHotKey(HWND::default(), RAYO_HOTKEY_ID);
        }
    });
}

fn selected_path(ui: &MainWindow, paths: &Arc<Mutex<Vec<String>>>) -> Option<String> {
    let idx = ui.get_selected_index();
    if idx < 0 {
        return None;
    }
    let idx = idx as usize;
    let guard = paths.lock().ok()?;
    guard.get(idx).cloned()
}

fn clear_preview(ui: &MainWindow) {
    ui.set_preview_title("Select result".into());
    ui.set_preview_body("Preview appears here".into());
    ui.set_preview_has_image(false);
    ui.set_preview_image(slint::Image::default());
}

fn load_preview(path: &str) -> (String, String, Option<slint::Image>) {
    let preview_path = Path::new(path);
    if preview_path.is_dir() {
        return (
            path.to_string(),
            "Directory selected. Preview not available.".to_string(),
            None,
        );
    }
    let title = preview_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string();
    let extension = preview_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let image_extension = matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico"
    );
    if image_extension {
        match slint::Image::load_from_path(preview_path) {
            Ok(image) => return (title, path.to_string(), Some(image)),
            Err(err) => {
                return (
                    title,
                    format!("Image preview failed: {err}. Path: {path}"),
                    None,
                );
            }
        }
    }

    let mut file = match File::open(preview_path) {
        Ok(file) => file,
        Err(err) => {
            return (
                title,
                format!("Cannot open file preview: {err}. Path: {path}"),
                None,
            );
        }
    };
    let mut sample = vec![0u8; 8192];
    let read = match file.read(&mut sample) {
        Ok(read) => read,
        Err(err) => {
            return (
                title,
                format!("Cannot read file preview: {err}. Path: {path}"),
                None,
            );
        }
    };
    sample.truncate(read);
    if sample.contains(&0) {
        return (
            title,
            format!("Binary file preview not available.\n{path}"),
            None,
        );
    }

    let text = String::from_utf8_lossy(&sample);
    let lines = text
        .lines()
        .take(100)
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    if lines.is_empty() {
        return (title, "<empty file>".to_string(), None);
    }
    (title, lines, None)
}

fn update_preview_for_selected(ui: &MainWindow, paths: &Arc<Mutex<Vec<String>>>) {
    let Some(path) = selected_path(ui, paths) else {
        clear_preview(ui);
        return;
    };
    let (title, body, image) = load_preview(&path);
    ui.set_preview_title(title.into());
    ui.set_preview_body(body.into());
    if let Some(image) = image {
        ui.set_preview_has_image(true);
        ui.set_preview_image(image);
    } else {
        ui.set_preview_has_image(false);
        ui.set_preview_image(slint::Image::default());
    }
}

fn add_if_dir_exists(roots: &mut Vec<PathBuf>, path: PathBuf) {
    if path.is_dir() {
        roots.push(path);
    }
}

fn app_catalog_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(app_data) = std::env::var("APPDATA") {
        add_if_dir_exists(
            &mut roots,
            PathBuf::from(app_data).join("Microsoft\\Windows\\Start Menu\\Programs"),
        );
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        add_if_dir_exists(
            &mut roots,
            PathBuf::from(program_data).join("Microsoft\\Windows\\Start Menu\\Programs"),
        );
    }
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        add_if_dir_exists(
            &mut roots,
            PathBuf::from(local_app_data).join("Microsoft\\WindowsApps"),
        );
    }
    roots
}

fn should_include_app_file(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    matches!(extension.as_str(), "lnk" | "appref-ms" | "exe")
}

fn app_display_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?.trim();
    if stem.is_empty() || stem.to_ascii_lowercase().starts_with("uninstall") {
        return None;
    }
    Some(stem.to_string())
}

fn build_app_catalog() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for root in app_catalog_roots() {
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let read_dir = match fs::read_dir(&dir) {
                Ok(read_dir) => read_dir,
                Err(_) => continue,
            };
            for item in read_dir.flatten() {
                let path = item.path();
                let metadata = match item.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                if metadata.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !metadata.is_file() || !should_include_app_file(&path) {
                    continue;
                }
                let Some(display_name) = app_display_name(&path) else {
                    continue;
                };
                let launch_path = path.display().to_string();
                if seen.insert(launch_path.to_ascii_lowercase()) {
                    entries.push(AppEntry {
                        display_name,
                        launch_path,
                    });
                }
            }
        }
    }
    entries.sort_by(|left, right| {
        left.display_name
            .to_ascii_lowercase()
            .cmp(&right.display_name.to_ascii_lowercase())
    });
    entries
}

fn ensure_app_catalog(cache: &Arc<Mutex<AppCatalogCache>>) -> Vec<AppEntry> {
    let mut guard = match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    let fresh = guard
        .built_at
        .map(|built_at| now.duration_since(built_at) < APP_CATALOG_TTL)
        .unwrap_or(false);
    if !fresh || guard.entries.is_empty() {
        guard.entries = build_app_catalog();
        guard.built_at = Some(now);
    }
    guard.entries.clone()
}

fn search_apps(
    query: &str,
    limit: usize,
    seen_paths: &HashSet<String>,
    cache: &Arc<Mutex<AppCatalogCache>>,
) -> Vec<QueryResultDto> {
    if limit == 0 || query.trim().chars().count() < 2 {
        return Vec::new();
    }
    let query_lower = query.to_ascii_lowercase();
    let mut matches: Vec<(u8, usize, QueryResultDto)> = ensure_app_catalog(cache)
        .into_iter()
        .filter(|app| !seen_paths.contains(&app.launch_path.to_ascii_lowercase()))
        .filter_map(|app| {
            let name_lower = app.display_name.to_ascii_lowercase();
            let path_lower = app.launch_path.to_ascii_lowercase();
            if !name_lower.contains(&query_lower) && !path_lower.contains(&query_lower) {
                return None;
            }
            let starts = if name_lower.starts_with(&query_lower) {
                0
            } else {
                1
            };
            let len = app.display_name.len();
            Some((
                starts,
                len,
                QueryResultDto {
                    path: app.launch_path,
                    is_directory: false,
                    is_app: true,
                    line_number: None,
                    line_text: Some(format!("App • {}", app.display_name)),
                },
            ))
        })
        .collect();
    matches.sort_by(|(a_starts, a_len, a_item), (b_starts, b_len, b_item)| {
        a_starts
            .cmp(b_starts)
            .then_with(|| a_len.cmp(b_len))
            .then_with(|| a_item.path.cmp(&b_item.path))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(_, _, item)| item)
        .collect()
}

fn interleave_app_results(
    mut base: Vec<QueryResultDto>,
    apps: Vec<QueryResultDto>,
    limit: usize,
) -> Vec<QueryResultDto> {
    if apps.is_empty() {
        if base.len() > limit {
            base.truncate(limit);
        }
        return base;
    }
    let mut output = Vec::with_capacity(limit);
    let mut base_idx = 0usize;
    let mut app_idx = 0usize;
    while output.len() < limit && (base_idx < base.len() || app_idx < apps.len()) {
        for _ in 0..2 {
            if output.len() >= limit {
                break;
            }
            if let Some(item) = base.get(base_idx) {
                output.push(item.clone());
                base_idx += 1;
            }
        }
        if output.len() >= limit {
            break;
        }
        if let Some(item) = apps.get(app_idx) {
            output.push(item.clone());
            app_idx += 1;
        }
    }
    output
}

fn shell_icon_from_path(path: &str, is_directory: bool) -> Option<slint::Image> {
    let path_obj = Path::new(path);
    if path_obj.exists()
        && let Some(extension) = path_obj.extension().and_then(|value| value.to_str())
    {
        let ext = extension.to_ascii_lowercase();
        if matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "ico"
        ) && let Ok(image) = slint::Image::load_from_path(path_obj)
        {
            return Some(image);
        }
    }

    let mut fallbacks = Vec::new();
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(base_dir) = exe_path.parent()
    {
        if is_directory {
            fallbacks.push(base_dir.join("Images\\rayo.folder.png"));
        } else {
            fallbacks.push(base_dir.join("Images\\rayo.file.png"));
        }
        fallbacks.push(base_dir.join("assets\\rayo.png"));
    }
    fallbacks.push(PathBuf::from(
        "c:\\src\\rayo\\integrations\\powertoys-run\\Images\\rayo.file.png",
    ));
    fallbacks.push(PathBuf::from(
        "c:\\src\\rayo\\crates\\rayo-gui\\ui\\assets\\rayo.png",
    ));

    for fallback in fallbacks {
        if fallback.exists()
            && let Ok(image) = slint::Image::load_from_path(&fallback)
        {
            return Some(image);
        }
    }
    None
}

fn icon_cache_key(path: &str, is_directory: bool, is_app: bool, line_match: bool) -> String {
    if is_app {
        let ext = Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("app");
        return format!("app:{}", ext.to_ascii_lowercase());
    }
    if is_directory {
        return "dir".to_string();
    }
    if line_match {
        let ext = Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("line");
        return format!("line:{}", ext.to_ascii_lowercase());
    }
    let ext = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    format!("file:{}", ext.to_ascii_lowercase())
}

fn resolve_ui_row_icon(row: &UiRowData) -> slint::Image {
    let key = icon_cache_key(&row.path, row.is_directory, row.is_app, row.line_match);
    UI_ICON_CACHE.with(|cache_cell| {
        let mut cache = cache_cell.borrow_mut();
        if let Some(image) = cache.get(&key) {
            return image.clone();
        }
        let resolved = shell_icon_from_path(&row.path, row.is_directory).unwrap_or_default();
        cache.insert(key, resolved.clone());
        resolved
    })
}

fn run_search(
    config: &GuiConfig,
    settings_state: &Arc<RwLock<GuiSettings>>,
    fallback_index: &Arc<RwLock<Option<FileIndex>>>,
    app_catalog: &Arc<Mutex<AppCatalogCache>>,
    query: String,
    pipe_client: &mut Option<PipeClient>,
) -> UiPayload {
    let settings = read_settings(settings_state);
    let trimmed_query = query.trim().to_string();
    if settings.content_mode && settings.under_dir.is_none() {
        return UiPayload {
            rows: Vec::new(),
            paths: Vec::new(),
            status_text: "Content mode needs scope folder in Settings.".to_string(),
            mode_text: "idle".to_string(),
        };
    }
    if trimmed_query.chars().count() < 2 && settings.under_dir.is_none() {
        return UiPayload {
            rows: Vec::new(),
            paths: Vec::new(),
            status_text: "Type at least 2 characters to search.".to_string(),
            mode_text: "idle".to_string(),
        };
    }

    let request = QueryRequest {
        query: trimmed_query.clone(),
        extension: settings.extension.clone(),
        under_dir: settings.under_dir.clone(),
        glob: None,
        mode: if settings.content_mode {
            Some("content".to_string())
        } else {
            Some("name".to_string())
        },
        timeout_ms: if settings.content_mode {
            Some(3_000)
        } else {
            None
        },
        directories_only: settings.directories_only,
        files_only: settings.files_only,
        fuzzy: settings.fuzzy_mode,
        limit: Some(settings.limit),
    };

    let (raw_results, took_ms, total_entries, mode_text) = match query_service(
        &config.pipe_name,
        &request,
        pipe_client,
    ) {
        Ok(response) => {
            if response.status.as_deref() == Some("starting") {
                let scanned = response.indexed_entries.unwrap_or(0);
                return UiPayload {
                    rows: Vec::new(),
                    paths: Vec::new(),
                    status_text: format!("Service starting... indexed={scanned}"),
                    mode_text: "service (starting)".to_string(),
                };
            }
            (
                response.results,
                response.took_ms,
                response.total_entries,
                if let Some(metrics) = response.metrics {
                    format!(
                        "service | indexed={} req={} avg={:.2}ms",
                        metrics.indexed_entries, metrics.requests_total, metrics.avg_took_ms
                    )
                } else {
                    "service".to_string()
                },
            )
        }
        Err(service_err) => {
            let started = Instant::now();
            let total_entries = match ensure_fallback_index_loaded(config, fallback_index) {
                Ok(total_entries) => total_entries,
                Err(err) => {
                    return UiPayload {
                        rows: Vec::new(),
                        paths: Vec::new(),
                        status_text: format!(
                            "Service unavailable ({service_err}). Fallback failed: {err}"
                        ),
                        mode_text: "error".to_string(),
                    };
                }
            };
            let guard = match fallback_index.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let Some(index) = guard.as_ref() else {
                return UiPayload {
                    rows: Vec::new(),
                    paths: Vec::new(),
                    status_text: "Fallback index unavailable.".to_string(),
                    mode_text: "error".to_string(),
                };
            };
            if settings.content_mode {
                match search_content(&ContentSearchOptions {
                    query: trimmed_query.clone(),
                    under_dir: settings.under_dir.clone().map(PathBuf::from),
                    extension: settings.extension.clone(),
                    limit: settings.limit,
                    timeout: Duration::from_secs(3),
                }) {
                    Ok(content) => (
                        content
                            .matches
                            .into_iter()
                            .map(|item| QueryResultDto {
                                path: item.path,
                                is_directory: false,
                                is_app: false,
                                line_number: Some(item.line_number),
                                line_text: Some(item.line_text),
                            })
                            .collect::<Vec<_>>(),
                        content.took.as_millis(),
                        total_entries,
                        format!("fallback-content ({service_err})"),
                    ),
                    Err(err) => {
                        return UiPayload {
                            rows: Vec::new(),
                            paths: Vec::new(),
                            status_text: format!(
                                "Service unavailable ({service_err}). Content fallback failed: {err}"
                            ),
                            mode_text: "error".to_string(),
                        };
                    }
                }
            } else {
                let search_results = index.search(&SearchOptions {
                    query: trimmed_query.clone(),
                    extension: settings.extension.clone(),
                    under_dir: settings.under_dir.clone(),
                    exclude_prefixes: Vec::new(),
                    glob: None,
                    directories_only: settings.directories_only,
                    files_only: settings.files_only,
                    limit: settings.limit,
                    prefer_trigram: false,
                    fuzzy: settings.fuzzy_mode,
                });
                (
                    search_results
                        .into_iter()
                        .map(|item| QueryResultDto {
                            path: item.path,
                            is_directory: item.is_directory,
                            is_app: false,
                            line_number: None,
                            line_text: None,
                        })
                        .collect::<Vec<_>>(),
                    started.elapsed().as_millis(),
                    total_entries,
                    format!("fallback ({service_err})"),
                )
            }
        }
    };

    let merged_results = if settings.content_mode || !settings.apps_mode {
        raw_results
    } else {
        let seen_paths = raw_results
            .iter()
            .map(|item| item.path.to_ascii_lowercase())
            .collect::<HashSet<_>>();
        let app_limit = settings.limit.saturating_div(3).clamp(3, 12);
        let app_results = search_apps(&trimmed_query, app_limit, &seen_paths, app_catalog);
        interleave_app_results(raw_results, app_results, settings.limit)
    };

    let mut rows = Vec::with_capacity(merged_results.len());
    let mut paths = Vec::with_capacity(merged_results.len());
    for item in merged_results {
        let name = std::path::Path::new(&item.path)
            .file_name()
            .and_then(|part| part.to_str())
            .map(|part| part.to_string())
            .unwrap_or_else(|| item.path.clone());
        let kind = if item.is_app {
            "APP"
        } else if item.line_number.is_some() {
            "LINE"
        } else if item.is_directory {
            "DIR"
        } else {
            "FILE"
        };
        let subtitle = if let Some(line_number) = item.line_number {
            let excerpt = item
                .line_text
                .as_deref()
                .map(|line| line.trim())
                .filter(|line| !line.is_empty())
                .unwrap_or("<empty>");
            format!("{}:{}  {}", item.path, line_number, excerpt)
        } else if item.is_app {
            item.line_text
                .clone()
                .unwrap_or_else(|| format!("App • {}", item.path))
        } else {
            item.path.clone()
        };
        rows.push(UiRowData {
            kind: kind.to_string(),
            name,
            path: item.path.clone(),
            subtitle,
            is_directory: item.is_directory,
            is_app: item.is_app,
            line_match: item.line_number.is_some(),
        });
        paths.push(item.path);
    }

    UiPayload {
        status_text: format!("{} results in {} ms", rows.len(), took_ms),
        mode_text: format!("{mode_text} | indexed={total_entries}"),
        rows,
        paths,
    }
}

fn trigger_search(
    ui_weak: slint::Weak<MainWindow>,
    latest_request: Arc<AtomicU64>,
    search_tx: mpsc::Sender<SearchCommand>,
) {
    let Some(ui) = ui_weak.upgrade() else {
        return;
    };
    let query = ui.get_query().to_string();
    let request_id = latest_request.fetch_add(1, Ordering::SeqCst) + 1;
    let _ = search_tx.send(SearchCommand { request_id, query });
}

fn spawn_search_worker(
    ui_weak: slint::Weak<MainWindow>,
    config: Arc<GuiConfig>,
    settings_state: Arc<RwLock<GuiSettings>>,
    fallback_index: Arc<RwLock<Option<FileIndex>>>,
    app_catalog: Arc<Mutex<AppCatalogCache>>,
    latest_request: Arc<AtomicU64>,
    paths: Arc<Mutex<Vec<String>>>,
) -> mpsc::Sender<SearchCommand> {
    let (tx, rx) = mpsc::channel::<SearchCommand>();
    thread::spawn(move || {
        let mut pipe_client = None;
        while let Ok(mut command) = rx.recv() {
            while let Ok(next) = rx.try_recv() {
                command = next;
            }

            let payload = run_search(
                &config,
                &settings_state,
                &fallback_index,
                &app_catalog,
                command.query,
                &mut pipe_client,
            );
            let request_id = command.request_id;
            let latest_apply = latest_request.clone();
            let paths_apply = paths.clone();
            let ui_weak_apply = ui_weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if latest_apply.load(Ordering::SeqCst) != request_id {
                    return;
                }
                if let Some(ui) = ui_weak_apply.upgrade() {
                    apply_payload(&ui, payload, &paths_apply);
                }
            });
        }
    });
    tx
}

fn main() -> Result<()> {
    let _single_instance_guard = match acquire_single_instance()? {
        Some(handle) => handle,
        None => {
            focus_existing_rayo_window();
            return Ok(());
        }
    };
    spawn_global_hotkey_listener();

    let cli = Cli::parse();
    let mut startup_query = cli.query.clone();
    let (mut initial_settings, startup_notice) = load_settings().unwrap_or_else(|err| {
        eprintln!("failed to load settings: {err:#}");
        (GuiSettings::default(), None)
    });
    if let Some(under) = cli.under {
        initial_settings.under_dir = normalize_optional_text(under);
    }
    if let Some(open_path) = cli.open.as_ref() {
        apply_open_context(
            open_path,
            &mut initial_settings.under_dir,
            &mut startup_query,
        );
    }
    if let Some(limit) = cli.limit {
        initial_settings.limit = limit;
    }
    if let Some(debounce_ms) = cli.debounce_ms {
        initial_settings.debounce_ms = debounce_ms;
    }
    sanitize_settings(&mut initial_settings);
    if initial_settings.theme_auto {
        initial_settings.light_theme = detect_system_light_theme();
    }

    let config = Arc::new(GuiConfig {
        index_path: cli.index,
        pipe_name: cli.pipe,
    });
    let settings_state = Arc::new(RwLock::new(initial_settings.clone()));
    let fallback_index: Arc<RwLock<Option<FileIndex>>> = Arc::new(RwLock::new(None));
    let app_catalog: Arc<Mutex<AppCatalogCache>> = Arc::new(Mutex::new(AppCatalogCache::default()));
    let latest_request = Arc::new(AtomicU64::new(0));
    let result_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let ui = MainWindow::new().map_err(|err| anyhow!("failed to create GUI: {err}"))?;
    ui.set_under_text(
        initial_settings
            .under_dir
            .clone()
            .unwrap_or_else(|| "<none>".to_string())
            .into(),
    );
    ui.set_settings_under_dir(
        initial_settings
            .under_dir
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_settings_extension(
        initial_settings
            .extension
            .clone()
            .unwrap_or_default()
            .into(),
    );
    ui.set_settings_files_only(initial_settings.files_only);
    ui.set_settings_directories_only(initial_settings.directories_only);
    ui.set_settings_limit(initial_settings.limit as i32);
    ui.set_settings_debounce_ms(initial_settings.debounce_ms as i32);
    ui.set_settings_content_mode(initial_settings.content_mode);
    ui.set_settings_fuzzy_mode(initial_settings.fuzzy_mode);
    ui.set_settings_apps_mode(initial_settings.apps_mode);
    ui.set_settings_theme_auto(initial_settings.theme_auto);
    ui.set_settings_light_theme(initial_settings.light_theme);
    ui.set_theme_light(initial_settings.light_theme);
    ui.set_settings_hint(settings_hint(&initial_settings).into());
    if let Some(notice) = startup_notice {
        ui.set_status_text(notice.into());
    }
    if let Some(query) = startup_query {
        ui.set_query(query.into());
    }

    {
        let preload_config = config.clone();
        let preload_index = fallback_index.clone();
        let preload_ui = ui.as_weak();
        thread::spawn(move || {
            if can_connect_pipe(&preload_config.pipe_name) {
                return;
            }

            let preload_ui_loading = preload_ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = preload_ui_loading.upgrade()
                    && ui.get_query().trim().is_empty()
                {
                    ui.set_mode_text("fallback (preloading)".into());
                    ui.set_status_text("Loading local index...".into());
                }
            });

            let preload_result = ensure_fallback_index_loaded(&preload_config, &preload_index)
                .map_err(|err| err.to_string());
            let preload_ui_done = preload_ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = preload_ui_done.upgrade()
                    && ui.get_query().trim().is_empty()
                {
                    match preload_result {
                        Ok(total_entries) => {
                            ui.set_mode_text(format!("fallback | indexed={total_entries}").into());
                            ui.set_status_text("Type at least 2 characters to search.".into());
                        }
                        Err(err) => {
                            ui.set_mode_text("error".into());
                            ui.set_status_text(format!("Fallback preload failed: {err}").into());
                        }
                    }
                }
            });
        });
    }

    {
        let update_ui = ui.as_weak();
        thread::spawn(move || {
            if let Some(message) = maybe_check_updates() {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = update_ui.upgrade() {
                        ui.set_status_text(message.into());
                    }
                });
            }
        });
    }

    let debounce_timer = Rc::new(RefCell::new(Timer::default()));
    let search_tx = spawn_search_worker(
        ui.as_weak(),
        config.clone(),
        settings_state.clone(),
        fallback_index.clone(),
        app_catalog.clone(),
        latest_request.clone(),
        result_paths.clone(),
    );

    {
        let ui_weak = ui.as_weak();
        let timer = debounce_timer.clone();
        let settings_state = settings_state.clone();
        let latest_request = latest_request.clone();
        let search_tx = search_tx.clone();
        ui.on_query_edited(move |_text| {
            let ui_weak2 = ui_weak.clone();
            let latest2 = latest_request.clone();
            let search_tx2 = search_tx.clone();
            let debounce_ms = {
                let settings = read_settings(&settings_state);
                if settings.content_mode {
                    settings.debounce_ms.max(500)
                } else {
                    settings.debounce_ms
                }
            };
            timer.borrow_mut().start(
                TimerMode::SingleShot,
                Duration::from_millis(debounce_ms),
                move || {
                    trigger_search(ui_weak2.clone(), latest2.clone(), search_tx2.clone());
                },
            );
        });
    }

    {
        let ui_weak = ui.as_weak();
        let latest_request = latest_request.clone();
        let search_tx = search_tx.clone();
        ui.on_clear_query(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_query("".into());
                ui.set_selected_index(-1);
            }
            trigger_search(ui_weak.clone(), latest_request.clone(), search_tx.clone());
        });
    }

    {
        let ui_weak = ui.as_weak();
        let latest_request = latest_request.clone();
        let search_tx = search_tx.clone();
        ui.on_search_now(move || {
            trigger_search(ui_weak.clone(), latest_request.clone(), search_tx.clone());
        });
    }

    {
        let ui_weak = ui.as_weak();
        let settings_state = settings_state.clone();
        let latest_request = latest_request.clone();
        let search_tx = search_tx.clone();
        ui.on_search_mode_changed(move |content_mode| {
            {
                let mut guard = match settings_state.write() {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
                guard.content_mode = content_mode;
                let _ = save_settings(&guard);
            }
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_content_mode(content_mode);
                let current_settings = read_settings(&settings_state);
                ui.set_settings_hint(settings_hint(&current_settings).into());
            }
            trigger_search(ui_weak.clone(), latest_request.clone(), search_tx.clone());
        });
    }

    {
        let ui_weak = ui.as_weak();
        let settings_state = settings_state.clone();
        ui.on_open_settings(move || {
            let settings = read_settings(&settings_state);
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_under_dir(settings.under_dir.clone().unwrap_or_default().into());
                ui.set_settings_extension(settings.extension.clone().unwrap_or_default().into());
                ui.set_settings_files_only(settings.files_only);
                ui.set_settings_directories_only(settings.directories_only);
                ui.set_settings_limit(settings.limit as i32);
                ui.set_settings_debounce_ms(settings.debounce_ms as i32);
                ui.set_settings_content_mode(settings.content_mode);
                ui.set_settings_fuzzy_mode(settings.fuzzy_mode);
                ui.set_settings_apps_mode(settings.apps_mode);
                ui.set_settings_theme_auto(settings.theme_auto);
                ui.set_settings_light_theme(settings.light_theme);
                ui.set_settings_error_text("".into());
                ui.set_show_settings(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_cancel_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_settings(false);
                ui.set_settings_error_text("".into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_reset_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                let defaults = GuiSettings::default();
                ui.set_settings_under_dir("".into());
                ui.set_settings_extension("".into());
                ui.set_settings_files_only(defaults.files_only);
                ui.set_settings_directories_only(defaults.directories_only);
                ui.set_settings_limit(defaults.limit as i32);
                ui.set_settings_debounce_ms(defaults.debounce_ms as i32);
                ui.set_settings_content_mode(defaults.content_mode);
                ui.set_settings_fuzzy_mode(defaults.fuzzy_mode);
                ui.set_settings_apps_mode(defaults.apps_mode);
                ui.set_settings_theme_auto(defaults.theme_auto);
                ui.set_settings_light_theme(defaults.light_theme);
                ui.set_settings_error_text("".into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_clear_settings_filters(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_settings_under_dir("".into());
                ui.set_settings_extension("".into());
                ui.set_settings_files_only(false);
                ui.set_settings_directories_only(false);
                ui.set_settings_error_text("".into());
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let settings_state = settings_state.clone();
        let latest_request = latest_request.clone();
        let search_tx = search_tx.clone();
        ui.on_save_settings(
            move |under_dir,
                  extension,
                  files_only,
                  directories_only,
                  limit,
                  debounce_ms,
                  content_mode,
                  fuzzy_mode,
                  apps_mode,
                  theme_auto,
                  light_theme| {
                let mut next = GuiSettings {
                    under_dir: normalize_optional_text(under_dir.to_string()),
                    extension: normalize_optional_text(extension.to_string()),
                    files_only,
                    directories_only,
                    limit: limit.max(0) as usize,
                    debounce_ms: debounce_ms.max(0) as u64,
                    content_mode,
                    fuzzy_mode,
                    apps_mode,
                    theme_auto,
                    light_theme,
                };
                sanitize_settings(&mut next);
                if next.theme_auto {
                    next.light_theme = detect_system_light_theme();
                }
                if let Some(validation_error) = validate_settings(&next) {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_settings_error_text(validation_error.into());
                        ui.set_status_text("Please fix settings before saving.".into());
                    }
                    return;
                }

                {
                    let mut guard = match settings_state.write() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    *guard = next.clone();
                }

                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_under_text(
                        next.under_dir
                            .clone()
                            .unwrap_or_else(|| "<none>".to_string())
                            .into(),
                    );
                    ui.set_theme_light(next.light_theme);
                    ui.set_settings_hint(settings_hint(&next).into());
                    ui.set_show_settings(false);
                    ui.set_settings_error_text("".into());
                    match save_settings(&next) {
                        Ok(_) => ui.set_status_text("Settings saved.".into()),
                        Err(err) => {
                            ui.set_status_text(format!("Failed to save settings: {err}").into())
                        }
                    }
                }

                trigger_search(ui_weak.clone(), latest_request.clone(), search_tx.clone());
            },
        );
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_select_row(move |idx| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_index(idx);
                update_preview_for_selected(&ui, &paths);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_show_row_menu(move |idx| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_index(idx);
                ui.set_context_row_index(idx);
                ui.set_show_context_menu(true);
                update_preview_for_selected(&ui, &paths);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_hide_row_menu(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_context_menu(false);
                ui.set_context_row_index(-1);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_activate_row(move |idx| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_index(idx);
                if let Some(path) = selected_path(&ui, &paths) {
                    let _ = shell_open(&path, false);
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_open_selected(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_context_menu(false);
                ui.set_context_row_index(-1);
                if let Some(path) = selected_path(&ui, &paths) {
                    if let Err(err) = shell_open(&path, false) {
                        ui.set_status_text(format!("Open failed: {err}").into());
                    }
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_open_selected_admin(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_context_menu(false);
                ui.set_context_row_index(-1);
                if let Some(path) = selected_path(&ui, &paths) {
                    if let Err(err) = shell_open(&path, true) {
                        ui.set_status_text(format!("Open as admin failed: {err}").into());
                    }
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_open_folder_selected(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_context_menu(false);
                ui.set_context_row_index(-1);
                if let Some(path) = selected_path(&ui, &paths) {
                    if let Err(err) = open_containing_folder(&path) {
                        ui.set_status_text(format!("Open folder failed: {err}").into());
                    }
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let paths = result_paths.clone();
        ui.on_copy_path_selected(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_context_menu(false);
                ui.set_context_row_index(-1);
                if let Some(path) = selected_path(&ui, &paths) {
                    match Clipboard::new() {
                        Ok(mut clipboard) => {
                            if clipboard.set_text(path).is_ok() {
                                ui.set_status_text("Path copied.".into());
                            } else {
                                ui.set_status_text("Failed to copy path.".into());
                            }
                        }
                        Err(err) => {
                            ui.set_status_text(format!("Clipboard unavailable: {err}").into());
                        }
                    }
                }
            }
        });
    }

    trigger_search(ui.as_weak(), latest_request, search_tx);
    ui.run()
        .map_err(|err| anyhow!("failed to run GUI event loop: {err}"))?;
    Ok(())
}
