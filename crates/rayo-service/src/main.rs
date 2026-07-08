use std::ffi::c_void;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use rayo_core::{
    FileIndex, SearchOptions, is_running_as_admin, load_index, normalize_drive, save_index,
};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_MESSAGE, PIPE_TYPE_MESSAGE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
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
    #[arg(long, default_value = "C")]
    drive: String,
    #[arg(long)]
    drives: Option<String>,
    #[arg(long, default_value = "index.rayo")]
    index: PathBuf,
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
    query: String,
    extension: Option<String>,
    under_dir: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    directories_only: bool,
    #[serde(default)]
    files_only: bool,
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct QueryResultDto {
    path: String,
    is_directory: bool,
}

#[derive(Debug, Serialize)]
struct QueryResponse {
    took_ms: u128,
    total_entries: usize,
    results: Vec<QueryResultDto>,
}

#[derive(Debug, Default)]
struct ServiceMetrics {
    requests_total: u64,
    total_took_ms: u128,
    last_took_ms: u128,
    max_took_ms: u128,
}

#[derive(Clone)]
struct DriveState {
    drive: String,
    index_path: PathBuf,
    index: Arc<RwLock<FileIndex>>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    require_admin()?;
    let drives = parse_drives(&cli.drive, cli.drives.as_deref())?;
    let multi_drive = drives.len() > 1;
    let mut drive_states = Vec::with_capacity(drives.len());
    for drive in drives {
        let index_path = drive_index_path(&cli.index, &drive, multi_drive);
        let mut index = load_or_build_index(&drive, &index_path)?;
        if cli.trigram {
            let trigram_started = Instant::now();
            index.set_trigram_enabled(true);
            println!(
                "Trigram index enabled for {} in {:?}.",
                drive,
                trigram_started.elapsed()
            );
        }
        save_index(&index, &index_path)
            .with_context(|| format!("failed to save bootstrap index at {}", index_path.display()))?;
        println!(
            "Service bootstrap ready on {} with {} entries ({})",
            drive,
            index.entries.len(),
            index_path.display()
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
    let metrics = Arc::new(Mutex::new(ServiceMetrics::default()));
    let running = Arc::new(AtomicBool::new(true));
    let running_handler = running.clone();
    ctrlc::set_handler(move || {
        running_handler.store(false, Ordering::SeqCst);
    })
    .context("failed to install Ctrl+C handler")?;

    let watch_poll = cli.poll_ms.max(50);
    let watch_persist_every = cli.persist_every_changes.max(1);
    let mut watch_threads = Vec::new();
    for state in drive_states.iter() {
        let watch_index = state.index.clone();
        let watch_running = running.clone();
        let watch_index_path = state.index_path.clone();
        let watch_drive = state.drive.clone();
        watch_threads.push(thread::spawn(move || {
            println!("Watch loop started for {watch_drive}");
            run_watch_loop(
                watch_index,
                watch_running,
                watch_index_path,
                watch_poll,
                watch_persist_every,
            );
        }));
    }

    let metrics_running = running.clone();
    let metrics_indexes = indexes.clone();
    let metrics_state = metrics.clone();
    let metrics_interval = Duration::from_secs(cli.metrics_interval_secs.max(5));
    let metrics_thread = thread::spawn(move || {
        run_metrics_reporter(
            metrics_state,
            metrics_indexes,
            metrics_running,
            metrics_interval,
        );
    });

    println!("Named pipe listening on {PIPE_NAME}");
    let pipe_result = run_pipe_server(indexes, running.clone(), metrics, cli.default_limit.max(1));
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

fn load_or_build_index(drive: &str, index_path: &PathBuf) -> Result<FileIndex> {
    if index_path.exists() {
        let loaded = load_index(index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?;
        if loaded.drive.eq_ignore_ascii_case(drive) {
            println!("Loaded existing index from {}", index_path.display());
            return Ok(loaded);
        }
        println!(
            "Index drive mismatch ({} vs {}). Rebuilding index.",
            loaded.drive, drive
        );
    } else {
        println!("No index file found. Building initial index for {drive}.");
    }

    let started = Instant::now();
    let index = FileIndex::build(drive)?;
    println!(
        "Initial index built: {} entries in {:?}.",
        index.entries.len(),
        started.elapsed()
    );
    Ok(index)
}

fn run_watch_loop(
    index: Arc<RwLock<FileIndex>>,
    running: Arc<AtomicBool>,
    index_path: PathBuf,
    poll_ms: u64,
    persist_every_changes: usize,
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
                    eprintln!("watch loop error: {err:#}");
                }
            }
        }

        if changed_now > 0 {
            println!("Watch applied {changed_now} changes.");
        }
        if let Some(snapshot) = snapshot_to_save {
            if let Err(err) = save_index(&snapshot, &index_path) {
                eprintln!("failed to persist watch snapshot: {err:#}");
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
        eprintln!("failed to persist final snapshot: {err:#}");
    }
}

fn run_pipe_server(
    indexes: Arc<Vec<Arc<RwLock<FileIndex>>>>,
    running: Arc<AtomicBool>,
    metrics: Arc<Mutex<ServiceMetrics>>,
    default_limit: usize,
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

        let shared_indexes = indexes.clone();
        let shared_metrics = metrics.clone();
        let pipe_raw = pipe.0 as isize;
        thread::spawn(move || {
            let pipe = HANDLE(pipe_raw as *mut c_void);
            if let Err(err) = handle_pipe_client(pipe, shared_indexes, shared_metrics, default_limit)
            {
                eprintln!("pipe client error: {err:#}");
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
    indexes: Arc<Vec<Arc<RwLock<FileIndex>>>>,
    metrics: Arc<Mutex<ServiceMetrics>>,
    default_limit: usize,
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
        let mut options = SearchOptions {
            query: request.query,
            extension: request.extension,
            under_dir: request.under_dir,
            glob: request.glob,
            directories_only: request.directories_only,
            files_only: request.files_only,
            limit,
            prefer_trigram: false,
        };

        let started = Instant::now();
        let query_lower = options.query.to_ascii_lowercase();
        let mut total_entries = 0usize;
        let mut merged = Vec::new();
        for index in indexes.iter() {
            let guard = match index.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            total_entries += guard.entries.len();
            options.prefer_trigram = guard.trigram_enabled();
            merged.extend(guard.search(&options));
        }
        merged.sort_by(|a, b| compare_relevance_paths(&a.path, &b.path, &query_lower));
        if merged.len() > limit {
            merged.truncate(limit);
        }

        let took_ms = started.elapsed().as_millis();
        if let Ok(mut guard) = metrics.lock() {
            guard.requests_total += 1;
            guard.total_took_ms += took_ms;
            guard.last_took_ms = took_ms;
            guard.max_took_ms = guard.max_took_ms.max(took_ms);
        }
        let response = QueryResponse {
            took_ms,
            total_entries,
            results: merged
                .into_iter()
                .map(|item| QueryResultDto {
                    path: item.path,
                    is_directory: item.is_directory,
                })
                .collect(),
        };

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
    indexes: Arc<Vec<Arc<RwLock<FileIndex>>>>,
    running: Arc<AtomicBool>,
    interval: Duration,
) {
    while running.load(Ordering::SeqCst) {
        thread::sleep(interval);
        let entries = indexes
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
        println!(
            "[metrics] requests={} avg_ms={average_ms:.2} last_ms={} max_ms={} entries={entries}",
            snapshot.requests_total, snapshot.last_took_ms, snapshot.max_took_ms
        );
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
