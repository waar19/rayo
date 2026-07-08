#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use arboard::Clipboard;
use clap::Parser;
use rayo_core::{FileIndex, SearchOptions, load_index};
use serde::{Deserialize, Serialize};
use slint::{ModelRc, Timer, TimerMode, VecModel};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

slint::include_modules!();

const DEFAULT_PIPE: &str = r"\\.\pipe\rayo-query";

#[derive(Parser, Debug)]
#[command(author, version, about = "Rayo GUI search client")]
struct Cli {
    #[arg(long, default_value = "index.rayo")]
    index: PathBuf,
    #[arg(long)]
    under: Option<String>,
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
    directories_only: bool,
    files_only: bool,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryResultDto {
    path: String,
    is_directory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueryResponse {
    took_ms: u128,
    total_entries: usize,
    results: Vec<QueryResultDto>,
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
struct GuiSettings {
    under_dir: Option<String>,
    extension: Option<String>,
    files_only: bool,
    directories_only: bool,
    limit: usize,
    debounce_ms: u64,
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
        }
    }
}

struct UiPayload {
    rows: Vec<ResultRow>,
    paths: Vec<String>,
    status_text: String,
    mode_text: String,
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
    settings.debounce_ms = settings.debounce_ms.clamp(0, 200);
    if settings.files_only && settings.directories_only {
        settings.directories_only = false;
    }
}

fn settings_path() -> Result<PathBuf> {
    let app_data = std::env::var("APPDATA").context("APPDATA environment variable is not set")?;
    Ok(PathBuf::from(app_data).join("rayo").join("settings.json"))
}

fn load_settings() -> Result<GuiSettings> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(GuiSettings::default());
    }

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read settings file {}", path.display()))?;
    let mut settings: GuiSettings = serde_json::from_str(&raw)
        .with_context(|| format!("invalid settings JSON at {}", path.display()))?;
    sanitize_settings(&mut settings);
    Ok(settings)
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

fn read_settings(settings: &Arc<RwLock<GuiSettings>>) -> GuiSettings {
    match settings.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn apply_payload(ui: &MainWindow, payload: UiPayload, paths: &Arc<Mutex<Vec<String>>>) {
    let model = Rc::new(VecModel::from(payload.rows));
    ui.set_results(ModelRc::from(model));
    ui.set_status_text(payload.status_text.into());
    ui.set_mode_text(payload.mode_text.into());
    ui.set_selected_index(-1);
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

fn selected_path(ui: &MainWindow, paths: &Arc<Mutex<Vec<String>>>) -> Option<String> {
    let idx = ui.get_selected_index();
    if idx < 0 {
        return None;
    }
    let idx = idx as usize;
    let guard = paths.lock().ok()?;
    guard.get(idx).cloned()
}

fn run_search(
    config: &GuiConfig,
    settings_state: &Arc<RwLock<GuiSettings>>,
    fallback_index: &Arc<RwLock<Option<FileIndex>>>,
    query: String,
    pipe_client: &mut Option<PipeClient>,
) -> UiPayload {
    let settings = read_settings(settings_state);
    if query.trim().chars().count() < 2 && settings.under_dir.is_none() {
        return UiPayload {
            rows: Vec::new(),
            paths: Vec::new(),
            status_text: "Type at least 2 characters to search.".to_string(),
            mode_text: "idle".to_string(),
        };
    }

    let request = QueryRequest {
        query: query.clone(),
        extension: settings.extension.clone(),
        under_dir: settings.under_dir.clone(),
        glob: None,
        directories_only: settings.directories_only,
        files_only: settings.files_only,
        limit: Some(settings.limit),
    };

    let (raw_results, took_ms, total_entries, mode_text) =
        match query_service(&config.pipe_name, &request, pipe_client) {
            Ok(response) => (
                response.results,
                response.took_ms,
                response.total_entries,
                "service".to_string(),
            ),
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
                let search_results = index.search(&SearchOptions {
                    query,
                    extension: settings.extension.clone(),
                    under_dir: settings.under_dir.clone(),
                    glob: None,
                    directories_only: settings.directories_only,
                    files_only: settings.files_only,
                    limit: settings.limit,
                });
                (
                    search_results
                        .into_iter()
                        .map(|item| QueryResultDto {
                            path: item.path,
                            is_directory: item.is_directory,
                        })
                        .collect::<Vec<_>>(),
                    started.elapsed().as_millis(),
                    total_entries,
                    format!("fallback ({service_err})"),
                )
            }
        };

    let mut rows = Vec::with_capacity(raw_results.len());
    let mut paths = Vec::with_capacity(raw_results.len());
    for item in raw_results {
        let name = std::path::Path::new(&item.path)
            .file_name()
            .and_then(|part| part.to_str())
            .map(|part| part.to_string())
            .unwrap_or_else(|| item.path.clone());
        let kind = if item.is_directory { "DIR" } else { "FILE" };
        rows.push(ResultRow {
            kind: kind.into(),
            name: name.into(),
            path: item.path.clone().into(),
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
    let cli = Cli::parse();
    let mut initial_settings = load_settings().unwrap_or_else(|err| {
        eprintln!("failed to load settings: {err:#}");
        GuiSettings::default()
    });
    if let Some(under) = cli.under {
        initial_settings.under_dir = normalize_optional_text(under);
    }
    if let Some(limit) = cli.limit {
        initial_settings.limit = limit;
    }
    if let Some(debounce_ms) = cli.debounce_ms {
        initial_settings.debounce_ms = debounce_ms;
    }
    sanitize_settings(&mut initial_settings);

    let config = Arc::new(GuiConfig {
        index_path: cli.index,
        pipe_name: cli.pipe,
    });
    let settings_state = Arc::new(RwLock::new(initial_settings.clone()));
    let fallback_index: Arc<RwLock<Option<FileIndex>>> = Arc::new(RwLock::new(None));
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

    let debounce_timer = Rc::new(RefCell::new(Timer::default()));
    let search_tx = spawn_search_worker(
        ui.as_weak(),
        config.clone(),
        settings_state.clone(),
        fallback_index.clone(),
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
                settings.debounce_ms
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
        ui.on_search_now(move || {
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
                ui.set_show_settings(true);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        ui.on_cancel_settings(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_show_settings(false);
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let settings_state = settings_state.clone();
        let latest_request = latest_request.clone();
        let search_tx = search_tx.clone();
        ui.on_save_settings(
            move |under_dir, extension, files_only, directories_only, limit, debounce_ms| {
                let mut next = GuiSettings {
                    under_dir: normalize_optional_text(under_dir.to_string()),
                    extension: normalize_optional_text(extension.to_string()),
                    files_only,
                    directories_only,
                    limit: limit.max(0) as usize,
                    debounce_ms: debounce_ms.max(0) as u64,
                };
                sanitize_settings(&mut next);

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
                    ui.set_show_settings(false);
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
        ui.on_select_row(move |idx| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_index(idx);
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
