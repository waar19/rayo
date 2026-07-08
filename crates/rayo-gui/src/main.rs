#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
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
    #[arg(long, default_value_t = 200)]
    limit: usize,
    #[arg(long, default_value_t = 80)]
    debounce_ms: u64,
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

#[derive(Clone)]
struct GuiConfig {
    index_path: PathBuf,
    pipe_name: String,
    under_dir: Option<String>,
    limit: usize,
    debounce_ms: u64,
}

struct UiPayload {
    rows: Vec<ResultRow>,
    paths: Vec<String>,
    status_text: String,
    mode_text: String,
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

fn query_service(pipe_name: &str, request: &QueryRequest) -> Result<QueryResponse> {
    let mut stream = OpenOptions::new()
        .read(true)
        .write(true)
        .open(pipe_name)
        .with_context(|| format!("failed to connect service pipe {pipe_name}"))?;
    serde_json::to_writer(&mut stream, request).context("failed to serialize request")?;
    stream
        .write_all(b"\n")
        .context("failed to write request terminator")?;
    stream.flush().context("failed to flush request")?;

    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    let read = reader
        .read_line(&mut line)
        .context("failed to read response line")?;
    if read == 0 {
        return Err(anyhow!("service closed pipe without response"));
    }
    let response: QueryResponse =
        serde_json::from_str(line.trim_end()).context("invalid response JSON")?;
    Ok(response)
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
    fallback_index: &Arc<RwLock<Option<FileIndex>>>,
    query: String,
) -> UiPayload {
    if query.trim().chars().count() < 2 && config.under_dir.is_none() {
        return UiPayload {
            rows: Vec::new(),
            paths: Vec::new(),
            status_text: "Type at least 2 characters to search.".to_string(),
            mode_text: "idle".to_string(),
        };
    }

    let request = QueryRequest {
        query: query.clone(),
        extension: None,
        under_dir: config.under_dir.clone(),
        glob: None,
        directories_only: false,
        files_only: false,
        limit: Some(config.limit),
    };

    let (raw_results, took_ms, total_entries, mode_text) = match query_service(
        &config.pipe_name,
        &request,
    ) {
        Ok(response) => (
            response.results,
            response.took_ms,
            response.total_entries,
            "service".to_string(),
        ),
        Err(service_err) => {
            let started = Instant::now();
            let loaded = {
                let has_index = fallback_index
                    .read()
                    .ok()
                    .and_then(|guard| guard.as_ref().map(|_| true))
                    .unwrap_or(false);
                if !has_index {
                    let loaded = load_index(&config.index_path).with_context(|| {
                        format!(
                            "failed to load fallback index {}",
                            config.index_path.display()
                        )
                    });
                    match loaded {
                        Ok(index) => {
                            if let Ok(mut guard) = fallback_index.write() {
                                *guard = Some(index);
                            }
                        }
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
                    }
                }

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
                    extension: None,
                    under_dir: config.under_dir.clone(),
                    glob: None,
                    directories_only: false,
                    files_only: false,
                    limit: config.limit,
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
                    index.entries.len(),
                    format!("fallback ({service_err})"),
                )
            };
            loaded
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
    config: Arc<GuiConfig>,
    fallback_index: Arc<RwLock<Option<FileIndex>>>,
    latest_request: Arc<AtomicU64>,
    paths: Arc<Mutex<Vec<String>>>,
) {
    let Some(ui) = ui_weak.upgrade() else {
        return;
    };
    let query = ui.get_query().to_string();
    let request_id = latest_request.fetch_add(1, Ordering::SeqCst) + 1;
    ui.set_status_text("Searching...".into());

    thread::spawn(move || {
        let payload = run_search(&config, &fallback_index, query);
        let ui_weak_apply = ui_weak.clone();
        let latest_apply = latest_request.clone();
        let paths_apply = paths.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if latest_apply.load(Ordering::SeqCst) != request_id {
                return;
            }
            if let Some(ui) = ui_weak_apply.upgrade() {
                apply_payload(&ui, payload, &paths_apply);
            }
        });
    });
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Arc::new(GuiConfig {
        index_path: cli.index,
        pipe_name: cli.pipe,
        under_dir: cli.under,
        limit: cli.limit.max(1),
        debounce_ms: cli.debounce_ms.max(40),
    });
    let fallback_index: Arc<RwLock<Option<FileIndex>>> = Arc::new(RwLock::new(None));
    let latest_request = Arc::new(AtomicU64::new(0));
    let result_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let ui = MainWindow::new().map_err(|err| anyhow!("failed to create GUI: {err}"))?;
    ui.set_under_text(
        config
            .under_dir
            .clone()
            .unwrap_or_else(|| "<none>".to_string())
            .into(),
    );

    let debounce_timer = Rc::new(RefCell::new(Timer::default()));

    {
        let ui_weak = ui.as_weak();
        let timer = debounce_timer.clone();
        let config = config.clone();
        let fallback_index = fallback_index.clone();
        let latest_request = latest_request.clone();
        let result_paths = result_paths.clone();
        ui.on_query_edited(move |_text| {
            let ui_weak2 = ui_weak.clone();
            let config2 = config.clone();
            let fallback2 = fallback_index.clone();
            let latest2 = latest_request.clone();
            let paths2 = result_paths.clone();
            timer.borrow_mut().start(
                TimerMode::SingleShot,
                Duration::from_millis(config.debounce_ms),
                move || {
                    trigger_search(
                        ui_weak2.clone(),
                        config2.clone(),
                        fallback2.clone(),
                        latest2.clone(),
                        paths2.clone(),
                    );
                },
            );
        });
    }

    {
        let ui_weak = ui.as_weak();
        let config = config.clone();
        let fallback_index = fallback_index.clone();
        let latest_request = latest_request.clone();
        let result_paths = result_paths.clone();
        ui.on_search_now(move || {
            trigger_search(
                ui_weak.clone(),
                config.clone(),
                fallback_index.clone(),
                latest_request.clone(),
                result_paths.clone(),
            );
        });
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

    trigger_search(
        ui.as_weak(),
        config,
        fallback_index,
        latest_request,
        result_paths,
    );
    ui.run()
        .map_err(|err| anyhow!("failed to run GUI event loop: {err}"))?;
    Ok(())
}
