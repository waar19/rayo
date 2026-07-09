use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use rayo_core::{
    ContentSearchOptions, FileIndex, SearchOptions, is_running_as_admin, load_index,
    normalize_drive, save_index, search_content,
};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE, SYSTEMTIME,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::WindowsProgramming::DRIVE_FIXED;
use windows::core::{HRESULT, PCWSTR};

const PIPE_NAME: &str = r"\\.\pipe\rayo-query";
const PIPE_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;WD)";
static PIPE_SECURITY_DESCRIPTOR: OnceLock<isize> = OnceLock::new();

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Background Rayo service with live index and named pipe queries"
)]
struct Cli {
    #[arg(long, default_value = "auto")]
    drive: String,
    #[arg(long)]
    drives: Option<String>,
    #[arg(long)]
    index: Option<PathBuf>,
    #[arg(long)]
    log_file: Option<PathBuf>,
    #[arg(long, default_value_t = 300)]
    poll_ms: u64,
    #[arg(long, default_value_t = 500)]
    persist_every_changes: usize,
    #[arg(long, default_value_t = 100)]
    default_limit: usize,
    #[arg(long, default_value_t = false)]
    trigram: bool,
    #[arg(long, default_value_t = 30)]
    metrics_interval_secs: u64,
}

#[derive(Debug, Deserialize)]
struct QueryRequest {
    #[serde(default)]
    query: String,
    extension: Option<String>,
    under_dir: Option<String>,
    glob: Option<String>,
    mode: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    directories_only: bool,
    #[serde(default)]
    files_only: bool,
    #[serde(default)]
    fuzzy: bool,
    #[serde(default)]
    metrics: bool,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct QueryResultDto {
    path: String,
    is_directory: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_text: Option<String>,
}

#[derive(Debug, Serialize)]
struct QueryResponse {
    took_ms: u128,
    total_entries: usize,
    results: Vec<QueryResultDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    indexed_entries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics: Option<QueryMetricsDto>,
}

#[derive(Debug, Clone, Serialize)]
struct QueryMetricsDto {
    requests_total: u64,
    avg_took_ms: f64,
    last_took_ms: u128,
    max_took_ms: u128,
    indexed_entries: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestMode {
    Name,
    Content,
}

impl RequestMode {
    fn from_request(value: Option<&str>) -> Self {
        match value {
            Some(mode) if mode.eq_ignore_ascii_case("content") => Self::Content,
            _ => Self::Name,
        }
    }
}

impl QueryResponse {
    fn from_results(took_ms: u128, total_entries: usize, results: Vec<QueryResultDto>) -> Self {
        Self {
            took_ms,
            total_entries,
            results,
            status: None,
            indexed_entries: None,
            metrics: None,
        }
    }

    fn starting(indexed_entries: usize) -> Self {
        Self {
            took_ms: 0,
            total_entries: indexed_entries,
            results: Vec::new(),
            status: Some("starting"),
            indexed_entries: Some(indexed_entries),
            metrics: None,
        }
    }
}

#[derive(Debug, Default)]
struct ServiceMetrics {
    requests_total: u64,
    total_took_ms: u128,
    last_took_ms: u128,
    max_took_ms: u128,
}

#[derive(Default)]
struct ServiceState {
    indexes: RwLock<Vec<Arc<RwLock<FileIndex>>>>,
    ready: AtomicBool,
    indexed_entries: AtomicUsize,
}

impl ServiceState {
    fn update_indexes(&self, indexes: Vec<Arc<RwLock<FileIndex>>>) {
        let mut guard = match self.indexes.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = indexes;
    }

    fn snapshot_indexes(&self) -> Vec<Arc<RwLock<FileIndex>>> {
        let guard = match self.indexes.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.clone()
    }
}

#[derive(Clone)]
struct DriveState {
    drive: String,
    index_path: PathBuf,
    index: Arc<RwLock<FileIndex>>,
}

type SharedLog = Arc<Mutex<File>>;

struct BootstrapIndex {
    index: FileIndex,
    built_from_scratch: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    require_admin()?;
    let auto_drive_mode = cli
        .drives
        .as_deref()
        .unwrap_or(cli.drive.as_str())
        .eq_ignore_ascii_case("auto");
    let drives = parse_drives(&cli.drive, cli.drives.as_deref())?;
    let log_path = cli.log_file.unwrap_or_else(default_background_log_path);
    let logger = open_log_writer(&log_path)?;
    log_info(
        &logger,
        format!("Logging service output to {}", log_path.display()),
    );
    let index_base = cli.index.clone();
    let multi_drive = drives.len() > 1;
    let service_state = Arc::new(ServiceState::default());
    let metrics = Arc::new(Mutex::new(ServiceMetrics::default()));
    let running = Arc::new(AtomicBool::new(true));
    let running_handler = running.clone();
    ctrlc::set_handler(move || {
        running_handler.store(false, Ordering::SeqCst);
    })
    .context("failed to install Ctrl+C handler")?;

    log_info(&logger, format!("Named pipe listening on {PIPE_NAME}"));
    let pipe_running = running.clone();
    let pipe_state = service_state.clone();
    let pipe_metrics = metrics.clone();
    let pipe_logger = logger.clone();
    let pipe_default_limit = cli.default_limit.max(1);
    let pipe_thread = thread::spawn(move || {
        run_pipe_server(
            pipe_state,
            pipe_running,
            pipe_metrics,
            pipe_default_limit,
            pipe_logger,
        )
    });

    let mut drive_states = Vec::with_capacity(drives.len());
    let mut indexed_entries_accumulated = 0usize;
    for drive in drives {
        let index_path = match &index_base {
            Some(base) => drive_index_path(base, &drive, multi_drive),
            None => default_background_index_path_for_drive(&drive),
        };
        let bootstrap = load_or_build_index(
            &drive,
            &index_path,
            &logger,
            service_state.clone(),
            indexed_entries_accumulated,
        )?;
        let mut index = bootstrap.index;
        if cli.trigram {
            let trigram_started = Instant::now();
            index.set_trigram_enabled(true);
            log_info(
                &logger,
                format!(
                    "Trigram index enabled for {} in {:?}.",
                    drive,
                    trigram_started.elapsed()
                ),
            );
        }
        if bootstrap.built_from_scratch {
            save_index(&index, &index_path).with_context(|| {
                format!("failed to save bootstrap index at {}", index_path.display())
            })?;
        }
        indexed_entries_accumulated += index.entries.len();
        service_state
            .indexed_entries
            .store(indexed_entries_accumulated, Ordering::Relaxed);
        log_info(
            &logger,
            format!(
                "Service bootstrap ready on {} with {} entries ({})",
                drive,
                index.entries.len(),
                index_path.display()
            ),
        );
        drive_states.push(DriveState {
            drive,
            index_path,
            index: Arc::new(RwLock::new(index)),
        });
    }

    let drive_states = Arc::new(drive_states);
    let indexes = Arc::new(
        drive_states
            .iter()
            .map(|state| state.index.clone())
            .collect::<Vec<_>>(),
    );
    service_state.update_indexes(indexes.as_ref().clone());
    service_state.ready.store(true, Ordering::SeqCst);

    let watch_poll = cli.poll_ms.max(50);
    let watch_persist_every = cli.persist_every_changes.max(1);
    let mut watch_threads = Vec::new();
    for state in drive_states.iter() {
        let watch_index = state.index.clone();
        let watch_running = running.clone();
        let watch_index_path = state.index_path.clone();
        let watch_drive = state.drive.clone();
        let watch_logger = logger.clone();
        watch_threads.push(thread::spawn(move || {
            log_info(
                &watch_logger,
                format!("Watch loop started for {watch_drive}"),
            );
            run_watch_loop(
                watch_index,
                watch_running,
                watch_index_path,
                watch_poll,
                watch_persist_every,
                watch_logger,
            );
        }));
    }

    if auto_drive_mode {
        let mut known_indexes = std::collections::HashMap::new();
        for state in drive_states.iter() {
            known_indexes.insert(state.drive.clone(), state.index.clone());
        }
        let auto_running = running.clone();
        let auto_logger = logger.clone();
        let auto_state = service_state.clone();
        let auto_index_base = index_base.clone();
        let auto_trigram = cli.trigram;
        thread::spawn(move || {
            run_auto_drive_monitor(
                known_indexes,
                auto_state,
                auto_running,
                auto_logger,
                auto_index_base,
                auto_trigram,
            );
        });
    }

    let metrics_running = running.clone();
    let metrics_service_state = service_state.clone();
    let metrics_state = metrics.clone();
    let metrics_interval = Duration::from_secs(cli.metrics_interval_secs.max(5));
    let metrics_logger = logger.clone();
    let metrics_thread = thread::spawn(move || {
        run_metrics_reporter(
            metrics_state,
            metrics_service_state,
            metrics_running,
            metrics_interval,
            metrics_logger,
        );
    });

    let pipe_result = match pipe_thread.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow!("named pipe thread panicked")),
    };
    running.store(false, Ordering::SeqCst);
    for handle in watch_threads {
        let _ = handle.join();
    }
    let _ = metrics_thread.join();
    pipe_result
}

fn require_admin() -> Result<()> {
    if !is_running_as_admin() {
        return Err(anyhow!(
            "this command requires Administrator privileges to read MFT/USN Journal"
        ));
    }
    Ok(())
}

fn parse_drives(default_drive: &str, drives_arg: Option<&str>) -> Result<Vec<String>> {
    let raw = drives_arg.unwrap_or(default_drive);
    if raw.eq_ignore_ascii_case("auto") {
        let detected = detect_fixed_ntfs_drives()?;
        if detected.is_empty() {
            return Err(anyhow!("no NTFS fixed drives detected for --drives auto"));
        }
        return Ok(detected);
    }
    let mut drives = Vec::new();
    let mut seen = HashSet::new();
    for candidate in raw.split(',') {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }
        let normalized = normalize_drive(trimmed)?;
        if seen.insert(normalized.clone()) {
            drives.push(normalized);
        }
    }
    if drives.is_empty() {
        return Err(anyhow!("no valid drives provided"));
    }
    Ok(drives)
}

fn detect_fixed_ntfs_drives() -> Result<Vec<String>> {
    let bitmask = unsafe { GetLogicalDrives() };
    if bitmask == 0 {
        return Err(anyhow!("failed to enumerate logical drives"));
    }

    let mut drives = Vec::new();
    for letter in b'A'..=b'Z' {
        let bit = 1u32 << (letter - b'A');
        if (bitmask & bit) == 0 {
            continue;
        }
        let root = format!("{}:\\", letter as char);
        let root_w = to_utf16_null(&root);
        let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
        if drive_type != DRIVE_FIXED {
            continue;
        }

        let mut fs_name_buffer = vec![0u16; 64];
        let has_volume = unsafe {
            GetVolumeInformationW(
                PCWSTR(root_w.as_ptr()),
                None,
                None,
                None,
                None,
                Some(&mut fs_name_buffer),
            )
        };
        if has_volume.is_err() {
            continue;
        }

        let fs_name = String::from_utf16_lossy(&fs_name_buffer)
            .trim_matches('\0')
            .to_string();
        if fs_name.eq_ignore_ascii_case("NTFS") {
            drives.push(format!("{}:", letter as char));
        }
    }
    Ok(drives)
}

fn drive_index_path(base: &Path, drive: &str, multi_drive: bool) -> PathBuf {
    if !multi_drive {
        return base.to_path_buf();
    }

    let drive_lower = drive.trim_end_matches(':').to_ascii_lowercase();
    let ext = base
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("index");

    let file_name = if stem.len() == 1 && stem.chars().all(|ch| ch.is_ascii_alphabetic()) {
        if ext.is_empty() {
            drive_lower
        } else {
            format!("{drive_lower}.{ext}")
        }
    } else if ext.is_empty() {
        format!("{stem}-{drive_lower}")
    } else {
        format!("{stem}-{drive_lower}.{ext}")
    };

    base.with_file_name(file_name)
}

fn load_or_build_index(
    drive: &str,
    index_path: &PathBuf,
    logger: &SharedLog,
    service_state: Arc<ServiceState>,
    progress_offset: usize,
) -> Result<BootstrapIndex> {
    if index_path.exists() {
        let loaded = load_index(index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?;
        if loaded.drive.eq_ignore_ascii_case(drive) {
            service_state
                .indexed_entries
                .store(progress_offset + loaded.entries.len(), Ordering::Relaxed);
            log_info(
                logger,
                format!("Loaded existing index from {}", index_path.display()),
            );
            return Ok(BootstrapIndex {
                index: loaded,
                built_from_scratch: false,
            });
        }
        log_info(
            logger,
            format!(
                "Index drive mismatch ({} vs {}). Rebuilding index.",
                loaded.drive, drive
            ),
        );
    } else {
        log_info(
            logger,
            format!(
                "No index file found at {}. Building initial index for {} (this can take a few minutes on first run).",
                index_path.display(),
                drive
            ),
        );
    }

    let started = Instant::now();
    let index = build_index_with_progress(drive, logger, Some(service_state), progress_offset)?;
    log_info(
        logger,
        format!(
            "Initial index built: {} entries in {:?}.",
            index.entries.len(),
            started.elapsed()
        ),
    );
    Ok(BootstrapIndex {
        index,
        built_from_scratch: true,
    })
}

fn build_index_with_progress(
    drive: &str,
    logger: &SharedLog,
    service_state: Option<Arc<ServiceState>>,
    progress_offset: usize,
) -> Result<FileIndex> {
    let progress = Arc::new(AtomicUsize::new(0));
    let monitor_running = Arc::new(AtomicBool::new(true));
    let monitor_progress = progress.clone();
    let monitor_flag = monitor_running.clone();
    let monitor_drive = drive.to_string();
    let monitor_logger = logger.clone();
    let monitor_state = service_state.clone();
    let started = Instant::now();

    let monitor = thread::spawn(move || {
        while monitor_flag.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(2));
            if !monitor_flag.load(Ordering::SeqCst) {
                break;
            }
            let scanned = monitor_progress.load(Ordering::Relaxed);
            if let Some(state) = &monitor_state {
                state
                    .indexed_entries
                    .store(progress_offset + scanned, Ordering::Relaxed);
            }
            log_info(
                &monitor_logger,
                format!(
                    "Indexing {} ... {} entries scanned (elapsed {:?})",
                    monitor_drive,
                    scanned,
                    started.elapsed()
                ),
            );
        }
    });

    let result = FileIndex::build_with_progress(drive, Some(progress.as_ref()));
    monitor_running.store(false, Ordering::SeqCst);
    let _ = monitor.join();
    if let Some(state) = service_state {
        let final_entries = result
            .as_ref()
            .map(|index| index.entries.len())
            .unwrap_or_else(|_| progress.load(Ordering::Relaxed));
        state
            .indexed_entries
            .store(progress_offset + final_entries, Ordering::Relaxed);
    }
    result
}

fn default_background_index_path_for_drive(drive: &str) -> PathBuf {
    let drive_lower = drive.trim_end_matches(':').to_ascii_lowercase();
    default_background_data_dir().join(format!("{drive_lower}.rayo"))
}

fn default_background_log_path() -> PathBuf {
    default_background_data_dir().join("service.log")
}

fn default_background_data_dir() -> PathBuf {
    env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Rayo")
}

fn open_log_writer(path: &Path) -> Result<SharedLog> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open log file {}", path.display()))?;
    Ok(Arc::new(Mutex::new(file)))
}

fn log_info(logger: &SharedLog, message: String) {
    let line = format!("{} {message}", current_timestamp());
    println!("{line}");
    if let Ok(mut file) = logger.lock() {
        let _ = writeln!(file, "{line}");
    }
}

fn log_error(logger: &SharedLog, message: String) {
    let line = format!("{} {message}", current_timestamp());
    eprintln!("{line}");
    if let Ok(mut file) = logger.lock() {
        let _ = writeln!(file, "{line}");
    }
}

fn current_timestamp() -> String {
    let local_time: SYSTEMTIME = unsafe { GetLocalTime() };
    format!(
        "[{:04}-{:02}-{:02} {:02}:{:02}:{:02}]",
        local_time.wYear,
        local_time.wMonth,
        local_time.wDay,
        local_time.wHour,
        local_time.wMinute,
        local_time.wSecond
    )
}

fn run_watch_loop(
    index: Arc<RwLock<FileIndex>>,
    running: Arc<AtomicBool>,
    index_path: PathBuf,
    poll_ms: u64,
    persist_every_changes: usize,
    logger: SharedLog,
) {
    let sleep = Duration::from_millis(poll_ms);
    let mut since_persist = 0usize;

    while running.load(Ordering::SeqCst) {
        let mut snapshot_to_save = None;
        let mut changed_now = 0usize;
        {
            let mut guard = match index.write() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.apply_journal_changes() {
                Ok(changed) => {
                    if changed > 0 {
                        changed_now = changed;
                        since_persist += changed;
                        if since_persist >= persist_every_changes {
                            since_persist = 0;
                            snapshot_to_save = Some(guard.clone());
                        }
                    }
                }
                Err(err) => {
                    log_error(&logger, format!("watch loop error: {err:#}"));
                }
            }
        }

        if changed_now > 0 {
            log_info(&logger, format!("Watch applied {changed_now} changes."));
        }
        if let Some(snapshot) = snapshot_to_save {
            if let Err(err) = save_index(&snapshot, &index_path) {
                log_error(
                    &logger,
                    format!("failed to persist watch snapshot: {err:#}"),
                );
            }
        }
        thread::sleep(sleep);
    }

    // Final flush on shutdown.
    let final_snapshot = match index.read() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    if let Err(err) = save_index(&final_snapshot, &index_path) {
        log_error(
            &logger,
            format!("failed to persist final snapshot: {err:#}"),
        );
    }
}

fn run_auto_drive_monitor(
    mut known_indexes: HashMap<String, Arc<RwLock<FileIndex>>>,
    service_state: Arc<ServiceState>,
    running: Arc<AtomicBool>,
    logger: SharedLog,
    index_base: Option<PathBuf>,
    trigram: bool,
) {
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(60));
        if !running.load(Ordering::SeqCst) {
            break;
        }

        let detected = match detect_fixed_ntfs_drives() {
            Ok(drives) => drives,
            Err(err) => {
                log_error(&logger, format!("auto-drive detect failed: {err:#}"));
                continue;
            }
        };
        let detected_set: HashSet<String> = detected.iter().cloned().collect();
        let known_set: HashSet<String> = known_indexes.keys().cloned().collect();

        let mut changed = false;
        for drive in detected_set.difference(&known_set) {
            let index_path = match &index_base {
                Some(base) => drive_index_path(base, drive, true),
                None => default_background_index_path_for_drive(drive),
            };
            let loaded = if index_path.exists() {
                match load_index(&index_path) {
                    Ok(mut index) => {
                        if index.drive.eq_ignore_ascii_case(drive) {
                            if trigram {
                                index.set_trigram_enabled(true);
                            }
                            Some(index)
                        } else {
                            None
                        }
                    }
                    Err(_) => None,
                }
            } else {
                None
            };

            let mut index = match loaded {
                Some(index) => index,
                None => match build_index_with_progress(drive, &logger, None, 0) {
                    Ok(mut index) => {
                        if trigram {
                            index.set_trigram_enabled(true);
                        }
                        if let Err(err) = save_index(&index, &index_path) {
                            log_error(
                                &logger,
                                format!(
                                    "failed to persist auto-detected drive {} index: {err:#}",
                                    drive
                                ),
                            );
                        }
                        index
                    }
                    Err(err) => {
                        log_error(
                            &logger,
                            format!(
                                "failed to build index for auto-detected drive {}: {err:#}",
                                drive
                            ),
                        );
                        continue;
                    }
                },
            };
            index.drive = drive.clone();
            known_indexes.insert(drive.clone(), Arc::new(RwLock::new(index)));
            log_info(&logger, format!("Auto-drive added: {drive}"));
            changed = true;
        }

        let removed: Vec<String> = known_set
            .difference(&detected_set)
            .map(|drive| drive.to_string())
            .collect();
        for drive in removed {
            known_indexes.remove(&drive);
            log_info(&logger, format!("Auto-drive removed: {drive}"));
            changed = true;
        }

        if changed {
            let indexes = known_indexes.values().cloned().collect::<Vec<_>>();
            let indexed_entries = indexes
                .iter()
                .map(|index| match index.read() {
                    Ok(guard) => guard.entries.len(),
                    Err(poisoned) => poisoned.into_inner().entries.len(),
                })
                .sum::<usize>();
            service_state.update_indexes(indexes);
            service_state
                .indexed_entries
                .store(indexed_entries, Ordering::Relaxed);
        }
    }
}

fn run_pipe_server(
    service_state: Arc<ServiceState>,
    running: Arc<AtomicBool>,
    metrics: Arc<Mutex<ServiceMetrics>>,
    default_limit: usize,
    logger: SharedLog,
) -> Result<()> {
    while running.load(Ordering::SeqCst) {
        let pipe = create_pipe_instance()?;
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        if let Err(err) = connected {
            if err.code() != HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) {
                unsafe {
                    let _ = CloseHandle(pipe);
                }
                return Err(anyhow!("failed to connect pipe client: {err}"));
            }
        }

        let shared_state = service_state.clone();
        let shared_metrics = metrics.clone();
        let shared_logger = logger.clone();
        let pipe_raw = pipe.0 as isize;
        thread::spawn(move || {
            let pipe = HANDLE(pipe_raw as *mut c_void);
            if let Err(err) = handle_pipe_client(
                pipe,
                shared_state,
                shared_metrics,
                default_limit,
                shared_logger.clone(),
            ) {
                log_error(&shared_logger, format!("pipe client error: {err:#}"));
            }
        });
    }
    Ok(())
}

fn create_pipe_instance() -> Result<HANDLE> {
    let pipe_name = to_utf16_null(PIPE_NAME);
    let security_attributes = pipe_security_attributes()?;
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(pipe_name.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            Some(&security_attributes as *const SECURITY_ATTRIBUTES),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(anyhow!("failed to create named pipe instance"));
    }
    Ok(handle)
}

fn pipe_security_attributes() -> Result<SECURITY_ATTRIBUTES> {
    if PIPE_SECURITY_DESCRIPTOR.get().is_none() {
        let sddl = to_utf16_null(PIPE_SDDL);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .context("failed to build named pipe security descriptor")?;
        }
        let _ = PIPE_SECURITY_DESCRIPTOR.set(descriptor.0 as isize);
    }
    let descriptor_ptr = *PIPE_SECURITY_DESCRIPTOR
        .get()
        .ok_or_else(|| anyhow!("pipe security descriptor not initialized"))?;

    Ok(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor_ptr as *mut c_void,
        bInheritHandle: false.into(),
    })
}

fn handle_pipe_client(
    pipe: HANDLE,
    service_state: Arc<ServiceState>,
    metrics: Arc<Mutex<ServiceMetrics>>,
    default_limit: usize,
    _logger: SharedLog,
) -> Result<()> {
    let owned = unsafe { OwnedHandle::from_raw_handle(pipe.0 as *mut _) };
    let mut stream = File::from(owned);
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("failed to clone client stream for read")?,
    );
    let mut request_line = String::new();
    loop {
        request_line.clear();
        let read = reader
            .read_line(&mut request_line)
            .context("failed to read request line")?;
        if read == 0 {
            break;
        }

        let request: QueryRequest =
            serde_json::from_str(request_line.trim_end()).context("invalid JSON request")?;
        let limit = request.limit.unwrap_or(default_limit).max(1);
        let query_mode = RequestMode::from_request(request.mode.as_deref());
        let content_timeout = Duration::from_millis(request.timeout_ms.unwrap_or(3_000).max(200));
        let indexed_entries_now = service_state.indexed_entries.load(Ordering::Relaxed);

        if request.metrics {
            let mut response = QueryResponse::from_results(0, indexed_entries_now, Vec::new());
            response.metrics = Some(snapshot_metrics(&metrics, indexed_entries_now));
            if !service_state.ready.load(Ordering::SeqCst) {
                response.status = Some("starting");
                response.indexed_entries = Some(indexed_entries_now);
            }
            serde_json::to_writer(&mut stream, &response)
                .context("failed to serialize metrics response")?;
            stream
                .write_all(b"\n")
                .context("failed to write metrics response terminator")?;
            stream.flush().context("failed to flush metrics response")?;
            continue;
        }

        if !service_state.ready.load(Ordering::SeqCst) {
            let response =
                QueryResponse::starting(service_state.indexed_entries.load(Ordering::Relaxed));
            serde_json::to_writer(&mut stream, &response)
                .context("failed to serialize startup response")?;
            stream
                .write_all(b"\n")
                .context("failed to write startup response terminator")?;
            stream.flush().context("failed to flush startup response")?;
            continue;
        }

        let indexes = service_state.snapshot_indexes();
        if indexes.is_empty() {
            let response =
                QueryResponse::starting(service_state.indexed_entries.load(Ordering::Relaxed));
            serde_json::to_writer(&mut stream, &response)
                .context("failed to serialize empty-index startup response")?;
            stream
                .write_all(b"\n")
                .context("failed to write empty-index startup response terminator")?;
            stream
                .flush()
                .context("failed to flush empty-index startup response")?;
            continue;
        }

        let started = Instant::now();
        let mut total_entries = 0usize;
        let mut results = Vec::new();
        for index in indexes.iter() {
            let guard = match index.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            total_entries += guard.entries.len();
        }
        match query_mode {
            RequestMode::Name => {
                let mut options = SearchOptions {
                    query: request.query,
                    extension: request.extension,
                    under_dir: request.under_dir,
                    glob: request.glob,
                    directories_only: request.directories_only,
                    files_only: request.files_only,
                    limit,
                    prefer_trigram: false,
                    fuzzy: request.fuzzy,
                };
                let query_lower = options.query.to_ascii_lowercase();
                let mut merged = Vec::new();
                for index in indexes.iter() {
                    let guard = match index.read() {
                        Ok(guard) => guard,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    options.prefer_trigram = guard.trigram_enabled();
                    merged.extend(guard.search(&options));
                }
                merged.sort_by(|a, b| compare_relevance_paths(&a.path, &b.path, &query_lower));
                if merged.len() > limit {
                    merged.truncate(limit);
                }
                results.extend(merged.into_iter().map(|item| QueryResultDto {
                    path: item.path,
                    is_directory: item.is_directory,
                    line_number: None,
                    line_text: None,
                }));
            }
            RequestMode::Content => {
                let query = request.query.trim().to_string();
                if !query.is_empty() {
                    let mut scopes = Vec::new();
                    if let Some(under_dir) = request.under_dir.as_ref() {
                        scopes.push(PathBuf::from(under_dir));
                    } else {
                        let mut seen = HashSet::new();
                        for index in indexes.iter() {
                            let guard = match index.read() {
                                Ok(guard) => guard,
                                Err(poisoned) => poisoned.into_inner(),
                            };
                            if seen.insert(guard.drive.clone()) {
                                scopes.push(PathBuf::from(format!("{}\\", guard.drive)));
                            }
                        }
                    }

                    let mut collected = Vec::new();
                    for scope in scopes {
                        if collected.len() >= limit || started.elapsed() >= content_timeout {
                            break;
                        }
                        let remaining_limit = limit - collected.len();
                        let remaining_timeout = content_timeout.saturating_sub(started.elapsed());
                        if remaining_timeout.is_zero() {
                            break;
                        }
                        let content_result = search_content(&ContentSearchOptions {
                            query: query.clone(),
                            under_dir: Some(scope),
                            extension: request.extension.clone(),
                            limit: remaining_limit,
                            timeout: remaining_timeout,
                        })?;
                        for item in content_result.matches {
                            if collected.len() >= limit {
                                break;
                            }
                            collected.push(QueryResultDto {
                                path: item.path,
                                is_directory: false,
                                line_number: Some(item.line_number),
                                line_text: Some(item.line_text),
                            });
                        }
                    }
                    results = collected;
                }
            }
        }

        let took_ms = started.elapsed().as_millis();
        if let Ok(mut guard) = metrics.lock() {
            guard.requests_total += 1;
            guard.total_took_ms += took_ms;
            guard.last_took_ms = took_ms;
            guard.max_took_ms = guard.max_took_ms.max(took_ms);
        }
        let mut response = QueryResponse::from_results(took_ms, total_entries, results);
        response.metrics = Some(snapshot_metrics(&metrics, total_entries));

        serde_json::to_writer(&mut stream, &response).context("failed to serialize response")?;
        stream
            .write_all(b"\n")
            .context("failed to write response terminator")?;
        stream.flush().context("failed to flush response")?;
    }
    Ok(())
}

fn run_metrics_reporter(
    metrics: Arc<Mutex<ServiceMetrics>>,
    service_state: Arc<ServiceState>,
    running: Arc<AtomicBool>,
    interval: Duration,
    logger: SharedLog,
) {
    while running.load(Ordering::SeqCst) {
        thread::sleep(interval);
        let entries = service_state
            .snapshot_indexes()
            .iter()
            .map(|index| match index.read() {
                Ok(guard) => guard.entries.len(),
                Err(poisoned) => poisoned.into_inner().entries.len(),
            })
            .sum::<usize>();
        let snapshot = match metrics.lock() {
            Ok(guard) => ServiceMetrics {
                requests_total: guard.requests_total,
                total_took_ms: guard.total_took_ms,
                last_took_ms: guard.last_took_ms,
                max_took_ms: guard.max_took_ms,
            },
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                ServiceMetrics {
                    requests_total: guard.requests_total,
                    total_took_ms: guard.total_took_ms,
                    last_took_ms: guard.last_took_ms,
                    max_took_ms: guard.max_took_ms,
                }
            }
        };

        let average_ms = if snapshot.requests_total == 0 {
            0.0
        } else {
            snapshot.total_took_ms as f64 / snapshot.requests_total as f64
        };
        log_info(
            &logger,
            format!(
                "[metrics] requests={} avg_ms={average_ms:.2} last_ms={} max_ms={} entries={entries}",
                snapshot.requests_total, snapshot.last_took_ms, snapshot.max_took_ms
            ),
        );
    }
}

fn snapshot_metrics(
    metrics: &Arc<Mutex<ServiceMetrics>>,
    indexed_entries: usize,
) -> QueryMetricsDto {
    let snapshot = match metrics.lock() {
        Ok(guard) => ServiceMetrics {
            requests_total: guard.requests_total,
            total_took_ms: guard.total_took_ms,
            last_took_ms: guard.last_took_ms,
            max_took_ms: guard.max_took_ms,
        },
        Err(poisoned) => {
            let guard = poisoned.into_inner();
            ServiceMetrics {
                requests_total: guard.requests_total,
                total_took_ms: guard.total_took_ms,
                last_took_ms: guard.last_took_ms,
                max_took_ms: guard.max_took_ms,
            }
        }
    };
    let avg_took_ms = if snapshot.requests_total == 0 {
        0.0
    } else {
        snapshot.total_took_ms as f64 / snapshot.requests_total as f64
    };
    QueryMetricsDto {
        requests_total: snapshot.requests_total,
        avg_took_ms,
        last_took_ms: snapshot.last_took_ms,
        max_took_ms: snapshot.max_took_ms,
        indexed_entries,
    }
}

fn compare_relevance_paths(a: &str, b: &str, query_lower: &str) -> std::cmp::Ordering {
    path_relevance_key(a, query_lower)
        .cmp(&path_relevance_key(b, query_lower))
        .then_with(|| a.cmp(b))
}

fn path_relevance_key(path: &str, query_lower: &str) -> (u8, usize, usize) {
    let file_name = path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    let starts_with = if file_name.starts_with(query_lower) {
        0
    } else {
        1
    };
    let match_pos = file_name.find(query_lower).unwrap_or(usize::MAX);
    (starts_with, match_pos, path.len())
}

fn to_utf16_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{compare_relevance_paths, drive_index_path};

    #[test]
    fn drive_index_path_uses_letter_file_names_for_letter_base() {
        let base = Path::new("c.rayo");
        assert_eq!(drive_index_path(base, "C:", true), Path::new("c.rayo"));
        assert_eq!(drive_index_path(base, "D:", true), Path::new("d.rayo"));
    }

    #[test]
    fn compare_relevance_paths_prioritizes_prefix_match() {
        let query = "ticket";
        let left = r"C:\src\ticketTrack.log";
        let right = r"C:\src\my-ticket-notes.log";
        let ordering = compare_relevance_paths(left, right, query);
        assert!(ordering.is_lt());
    }
}
