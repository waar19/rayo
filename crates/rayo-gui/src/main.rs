#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use eframe::egui;
use eframe::egui::Key;
use rayo_core::{FileIndex, SearchOptions, load_index};
use serde::{Deserialize, Serialize};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

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

struct RayoGuiApp {
    index_path: PathBuf,
    pipe_name: String,
    under_dir: Option<String>,
    limit: usize,
    debounce: Duration,
    query: String,
    last_query_sent: String,
    last_input_change: Instant,
    selected: usize,
    results: Vec<QueryResultDto>,
    status: String,
    fallback_index: Option<FileIndex>,
}

impl RayoGuiApp {
    fn new(cli: Cli) -> Self {
        Self {
            index_path: cli.index,
            pipe_name: cli.pipe,
            under_dir: cli.under,
            limit: cli.limit.max(1),
            debounce: Duration::from_millis(cli.debounce_ms.max(20)),
            query: String::new(),
            last_query_sent: "__init__".to_string(),
            last_input_change: Instant::now(),
            selected: 0,
            results: Vec::new(),
            status: "Ready.".to_string(),
            fallback_index: None,
        }
    }

    fn dispatch_search(&mut self) {
        let request = QueryRequest {
            query: self.query.clone(),
            extension: None,
            under_dir: self.under_dir.clone(),
            glob: None,
            directories_only: false,
            files_only: false,
            limit: Some(self.limit),
        };

        match query_service(&self.pipe_name, &request) {
            Ok(response) => {
                self.results = response.results;
                self.selected = self.selected.min(self.results.len().saturating_sub(1));
                self.status = format!(
                    "Service results: {} in {} ms ({} indexed entries)",
                    self.results.len(),
                    response.took_ms,
                    response.total_entries
                );
                self.last_query_sent = self.query.clone();
            }
            Err(pipe_err) => match self.query_fallback(&request) {
                Ok((results, took_ms, total_entries)) => {
                    self.results = results;
                    self.selected = self.selected.min(self.results.len().saturating_sub(1));
                    self.status = format!(
                        "Fallback results: {} in {} ms ({} indexed entries). Service unavailable: {}",
                        self.results.len(),
                        took_ms,
                        total_entries,
                        pipe_err
                    );
                    self.last_query_sent = self.query.clone();
                }
                Err(fallback_err) => {
                    self.status = format!(
                        "Search failed. Service error: {}. Fallback error: {}",
                        pipe_err, fallback_err
                    );
                }
            },
        }
    }

    fn query_fallback(&mut self, request: &QueryRequest) -> Result<(Vec<QueryResultDto>, u128, usize)> {
        if self.fallback_index.is_none() {
            let index = load_index(&self.index_path)
                .with_context(|| format!("failed to load fallback index {}", self.index_path.display()))?;
            self.fallback_index = Some(index);
        }
        let index = self
            .fallback_index
            .as_ref()
            .ok_or_else(|| anyhow!("fallback index unavailable"))?;
        let options = SearchOptions {
            query: request.query.clone(),
            extension: request.extension.clone(),
            under_dir: request.under_dir.clone(),
            glob: request.glob.clone(),
            directories_only: request.directories_only,
            files_only: request.files_only,
            limit: request.limit.unwrap_or(self.limit).max(1),
        };
        let started = Instant::now();
        let results = index.search(&options);
        let took_ms = started.elapsed().as_millis();
        let items = results
            .into_iter()
            .map(|item| QueryResultDto {
                path: item.path,
                is_directory: item.is_directory,
            })
            .collect();
        Ok((items, took_ms, index.entries.len()))
    }

    fn selected_path(&self) -> Option<&str> {
        self.results.get(self.selected).map(|item| item.path.as_str())
    }

    fn open_selected(&mut self, as_admin: bool) {
        if let Some(path) = self.selected_path() {
            if let Err(err) = shell_open(path, as_admin) {
                self.status = format!("Open failed: {err}");
            }
        }
    }
}

impl eframe::App for RayoGuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        ui.horizontal(|ui| {
            ui.label("Query:");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .desired_width(f32::INFINITY)
                    .hint_text("Type to search..."),
            );
            if response.changed() {
                self.last_input_change = Instant::now();
            }
            if ui.button("Search now").clicked() {
                self.last_query_sent.clear();
            }
        });

        ui.horizontal(|ui| {
            if ui.button("Open").clicked() {
                self.open_selected(false);
            }
            if ui.button("Open as admin").clicked() {
                self.open_selected(true);
            }
            if ui.button("Open folder").clicked() {
                if let Some(path) = self.selected_path() {
                    if let Err(err) = open_containing_folder(path) {
                        self.status = format!("Open folder failed: {err}");
                    }
                }
            }
            if ui.button("Copy path").clicked() {
                if let Some(path) = self.selected_path() {
                    ctx.copy_text(path.to_string());
                    self.status = "Path copied.".to_string();
                }
            }
        });
        ui.label(format!(
            "{} | under={}",
            self.status,
            self.under_dir.as_deref().unwrap_or("<none>")
        ));
        ui.separator();

        if self.query != self.last_query_sent && self.last_input_change.elapsed() >= self.debounce {
            self.dispatch_search();
        }

        if ctx.input(|i| i.key_pressed(Key::Enter) && i.modifiers.ctrl) {
            self.open_selected(true);
        } else if ctx.input(|i| i.key_pressed(Key::Enter)) {
            self.open_selected(false);
        }

        let row_height = 20.0;
        egui::ScrollArea::vertical().show_rows(ui, row_height, self.results.len(), |ui, range| {
            for idx in range {
                let item_path = self.results[idx].path.clone();
                let prefix = if self.results[idx].is_directory {
                    "DIR "
                } else {
                    "FILE"
                };
                let label = format!("[{prefix}] {item_path}");
                let response = ui.selectable_label(self.selected == idx, label);
                if response.clicked() {
                    self.selected = idx;
                }
                if response.double_clicked() {
                    self.open_selected(false);
                }

                response.context_menu(|ui| {
                    if ui.button("Open").clicked() {
                        let _ = shell_open(&item_path, false);
                        ui.close();
                    }
                    if ui.button("Open as admin").clicked() {
                        let _ = shell_open(&item_path, true);
                        ui.close();
                    }
                    if ui.button("Open folder").clicked() {
                        let _ = open_containing_folder(&item_path);
                        ui.close();
                    }
                    if ui.button("Copy path").clicked() {
                        ctx.copy_text(item_path.clone());
                        ui.close();
                    }
                });
            }
        });

        ctx.request_repaint_after(Duration::from_millis(16));
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "Rayo",
        options,
        Box::new(|_cc| Ok(Box::new(RayoGuiApp::new(cli)))),
    )
    .map_err(|err| anyhow!("failed to start GUI: {err}"))?;
    Ok(())
}
