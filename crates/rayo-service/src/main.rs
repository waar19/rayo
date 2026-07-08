use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use std::ffi::c_void;

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
#[command(author, version, about = "Background Rayo service with live index and named pipe queries")]
struct Cli {
    #[arg(long, default_value = "C")]
    drive: String,
    #[arg(long, default_value = "index.rayo")]
    index: PathBuf,
    #[arg(long, default_value_t = 300)]
    poll_ms: u64,
    #[arg(long, default_value_t = 500)]
    persist_every_changes: usize,
    #[arg(long, default_value_t = 100)]
    default_limit: usize,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    require_admin()?;
    let drive = normalize_drive(&cli.drive)?;

    let index = load_or_build_index(&drive, &cli.index)?;
    save_index(&index, &cli.index)
        .with_context(|| format!("failed to save bootstrap index at {}", cli.index.display()))?;
    println!(
        "Service bootstrap ready on {} with {} entries.",
        drive,
        index.entries.len()
    );

    let index = Arc::new(RwLock::new(index));
    let running = Arc::new(AtomicBool::new(true));
    let running_handler = running.clone();
    ctrlc::set_handler(move || {
        running_handler.store(false, Ordering::SeqCst);
    })
    .context("failed to install Ctrl+C handler")?;

    let watch_index = index.clone();
    let watch_running = running.clone();
    let watch_index_path = cli.index.clone();
    let watch_poll = cli.poll_ms.max(50);
    let watch_persist_every = cli.persist_every_changes.max(1);
    let watch_thread = thread::spawn(move || {
        run_watch_loop(
            watch_index,
            watch_running,
            watch_index_path,
            watch_poll,
            watch_persist_every,
        );
    });

    println!("Named pipe listening on {PIPE_NAME}");
    let pipe_result = run_pipe_server(index, running.clone(), cli.default_limit.max(1));
    running.store(false, Ordering::SeqCst);
    let _ = watch_thread.join();
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
    index: Arc<RwLock<FileIndex>>,
    running: Arc<AtomicBool>,
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

        let shared_index = index.clone();
        let pipe_raw = pipe.0 as isize;
        thread::spawn(move || {
            let pipe = HANDLE(pipe_raw as *mut c_void);
            if let Err(err) = handle_pipe_client(pipe, shared_index, default_limit) {
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

fn handle_pipe_client(pipe: HANDLE, index: Arc<RwLock<FileIndex>>, default_limit: usize) -> Result<()> {
    let owned = unsafe { OwnedHandle::from_raw_handle(pipe.0 as *mut _) };
    let mut stream = File::from(owned);
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .context("failed to clone client stream for read")?,
    );
    let mut request_line = String::new();
    let read = reader
        .read_line(&mut request_line)
        .context("failed to read request line")?;
    if read == 0 {
        return Ok(());
    }

    let request: QueryRequest =
        serde_json::from_str(request_line.trim_end()).context("invalid JSON request")?;
    let limit = request.limit.unwrap_or(default_limit).max(1);
    let options = SearchOptions {
        query: request.query,
        extension: request.extension,
        under_dir: request.under_dir,
        glob: request.glob,
        directories_only: request.directories_only,
        files_only: request.files_only,
        limit,
    };

    let started = Instant::now();
    let (results, total_entries) = {
        let guard = match index.read() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        (guard.search(&options), guard.entries.len())
    };
    let response = QueryResponse {
        took_ms: started.elapsed().as_millis(),
        total_entries,
        results: results
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
    Ok(())
}

fn to_utf16_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
