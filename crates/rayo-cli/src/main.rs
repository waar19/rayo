use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use rayo_core::{
    ContentSearchOptions, FileIndex, SearchOptions, SyntaxSearchOptions, is_running_as_admin,
    load_index, normalize_drive, save_index, search_content, search_syntax,
};
use winreg::RegKey;
use winreg::enums::HKEY_CURRENT_USER;

#[derive(Parser, Debug)]
#[command(author, version, about = "Ultra-fast NTFS file search for Windows")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Build or rebuild index from MFT/USN
    Index {
        #[arg(long, default_value = "C")]
        drive: String,
        #[arg(long, default_value = "index.rayo")]
        output: PathBuf,
        #[arg(long)]
        exclude: Option<String>,
    },
    /// Search inside an existing index
    Search {
        #[arg(long)]
        index: PathBuf,
        #[arg(long)]
        query: String,
        #[arg(long)]
        ext: Option<String>,
        #[arg(long)]
        under: Option<String>,
        #[arg(long)]
        glob: Option<String>,
        #[arg(long)]
        exclude: Option<String>,
        #[arg(long, default_value_t = false)]
        dirs_only: bool,
        #[arg(long, default_value_t = false)]
        files_only: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        trigram: bool,
        #[arg(long, default_value_t = false)]
        fuzzy: bool,
    },
    /// Search inside file contents (regex)
    Content {
        #[arg(long)]
        query: String,
        #[arg(long)]
        under: PathBuf,
        #[arg(long)]
        ext: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// Syntax-aware code search using tree-sitter
    Syntax {
        #[arg(long)]
        query: String,
        #[arg(long)]
        under: PathBuf,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        node_kind: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = 3000)]
        timeout_ms: u64,
    },
    /// Keep index up to date from USN Journal
    Watch {
        #[arg(long, default_value = "C")]
        drive: String,
        #[arg(long, default_value = "index.rayo")]
        index: PathBuf,
        #[arg(long)]
        exclude: Option<String>,
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,
    },
    /// Install or remove Explorer integration in current user registry
    Shell {
        #[command(subcommand)]
        action: ShellAction,
    },
    /// Install or manage the background Windows scheduled task
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ShellAction {
    Install {
        #[arg(long)]
        gui_path: Option<PathBuf>,
    },
    Uninstall,
    Doctor {
        #[arg(long)]
        gui_path: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ServiceAction {
    Install {
        #[arg(long)]
        service_exe: Option<PathBuf>,
        #[arg(long, default_value = "auto")]
        drives: String,
        #[arg(long)]
        index: Option<PathBuf>,
        #[arg(long)]
        log_file: Option<PathBuf>,
        #[arg(long)]
        exclude: Option<String>,
    },
    Uninstall,
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Index {
            drive,
            output,
            exclude,
        } => run_index(&drive, output, exclude),
        Commands::Search {
            index,
            query,
            ext,
            under,
            glob,
            exclude,
            dirs_only,
            files_only,
            limit,
            trigram,
            fuzzy,
        } => run_search(
            index, query, ext, under, glob, exclude, dirs_only, files_only, limit, trigram, fuzzy,
        ),
        Commands::Content {
            query,
            under,
            ext,
            limit,
        } => run_content_search(query, under, ext, limit),
        Commands::Syntax {
            query,
            under,
            language,
            node_kind,
            limit,
            timeout_ms,
        } => run_syntax_search(query, under, language, node_kind, limit, timeout_ms),
        Commands::Watch {
            drive,
            index,
            exclude,
            poll_ms,
        } => run_watch(&drive, index, exclude, poll_ms),
        Commands::Shell { action } => run_shell(action),
        Commands::Service { action } => run_service(action),
    }
}

fn run_index(drive: &str, output: PathBuf, exclude: Option<String>) -> Result<()> {
    require_admin()?;
    let drives = parse_drive_list(drive)?;
    let exclude_prefixes = parse_exclude_list(exclude.as_deref());
    let multi_drive = drives.len() > 1;
    let started = Instant::now();
    println!(
        "Starting index build on {} drive(s). This can take several minutes...",
        drives.len()
    );
    for drive in drives {
        let output_path = drive_index_path(&output, &drive, multi_drive);
        println!("Building index for {}...", drive);
        let build_started = Instant::now();
        let mut index = build_index_with_progress(&drive)?;
        let excluded = index.apply_exclude_prefixes(&exclude_prefixes);
        println!(
            "MFT/journal scan for {} completed in {:?}. Excluded {} entries. Saving index...",
            drive,
            build_started.elapsed(),
            excluded
        );
        let save_started = Instant::now();
        save_index(&index, &output_path)?;
        println!(
            "Index persisted for {} in {:?} -> {} (entries={})",
            drive,
            save_started.elapsed(),
            output_path.display(),
            index.entries.len()
        );
    }
    println!("Index build finished in {:?}", started.elapsed());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_search(
    index_path: PathBuf,
    query: String,
    ext: Option<String>,
    under: Option<String>,
    glob: Option<String>,
    exclude: Option<String>,
    dirs_only: bool,
    files_only: bool,
    limit: usize,
    trigram: bool,
    fuzzy: bool,
) -> Result<()> {
    let mut index = load_index(&index_path)?;
    index.set_trigram_enabled(trigram);
    let exclude_prefixes = parse_exclude_list(exclude.as_deref());
    let started = Instant::now();
    let results = index.search(&SearchOptions {
        query,
        extension: ext,
        under_dir: under,
        exclude_prefixes,
        glob,
        directories_only: dirs_only,
        files_only,
        limit,
        prefer_trigram: trigram,
        fuzzy,
    });

    for result in &results {
        let entry_type = if result.is_directory { "DIR " } else { "FILE" };
        println!("[{entry_type}] {}", result.path);
    }
    println!("Results: {} in {:?}", results.len(), started.elapsed());
    Ok(())
}

fn run_content_search(
    query: String,
    under: PathBuf,
    ext: Option<String>,
    limit: usize,
) -> Result<()> {
    let outcome = search_content(&ContentSearchOptions {
        query,
        under_dir: Some(under),
        extension: ext,
        limit,
        timeout: Duration::from_secs(3),
    })?;

    for item in &outcome.matches {
        println!("{}:{}:{}", item.path, item.line_number, item.line_text);
    }
    println!(
        "Content results: {} in {:?} (scanned={} timed_out={})",
        outcome.matches.len(),
        outcome.took,
        outcome.scanned_files,
        outcome.timed_out
    );
    Ok(())
}

fn run_syntax_search(
    query: String,
    under: PathBuf,
    language: Option<String>,
    node_kind: Option<String>,
    limit: usize,
    timeout_ms: u64,
) -> Result<()> {
    let outcome = search_syntax(&SyntaxSearchOptions {
        query,
        under_dir: Some(under),
        language,
        node_kind,
        limit,
        timeout: Duration::from_millis(timeout_ms.max(100)),
    })?;

    for item in &outcome.matches {
        println!(
            "{}:{}:{} [{}:{}] {}",
            item.path,
            item.line_number,
            item.column_number,
            item.language,
            item.node_kind,
            item.snippet
        );
    }
    println!(
        "Syntax results: {} in {:?} (scanned={} timed_out={})",
        outcome.matches.len(),
        outcome.took,
        outcome.scanned_files,
        outcome.timed_out
    );
    Ok(())
}

fn run_watch(
    drive: &str,
    index_path: PathBuf,
    exclude: Option<String>,
    poll_ms: u64,
) -> Result<()> {
    require_admin()?;
    let drive = normalize_drive(drive)?;
    let exclude_prefixes = parse_exclude_list(exclude.as_deref());
    println!(
        "Preparing watch on {}. Initial bootstrap can take time...",
        drive
    );
    let mut index = if index_path.exists() {
        println!("Loading existing index: {}", index_path.display());
        let loaded = load_index(&index_path)?;
        if loaded.drive.eq_ignore_ascii_case(&drive) {
            println!("Existing index is compatible ({})", loaded.drive);
            loaded
        } else {
            println!(
                "Existing index belongs to {}. Rebuilding for {}...",
                loaded.drive, drive
            );
            build_index_with_progress(&drive)?
        }
    } else {
        println!(
            "No index found at {}. Building initial index...",
            index_path.display()
        );
        build_index_with_progress(&drive)?
    };
    let excluded = index.apply_exclude_prefixes(&exclude_prefixes);
    if excluded > 0 {
        println!("Excluded {excluded} entries before starting watch.");
    }
    println!("Saving initial watch snapshot...");
    save_index(&index, &index_path)?;
    println!(
        "Watch started on {} ({} entries). Press Ctrl+C to exit.",
        drive,
        index.entries.len()
    );

    let running = Arc::new(AtomicBool::new(true));
    let running_handler = running.clone();
    ctrlc::set_handler(move || {
        running_handler.store(false, Ordering::SeqCst);
    })?;

    let sleep = Duration::from_millis(poll_ms.max(50));
    while running.load(Ordering::SeqCst) {
        let changed = index.apply_journal_changes()?;
        let excluded = index.apply_exclude_prefixes(&exclude_prefixes);
        if changed > 0 || excluded > 0 {
            save_index(&index, &index_path)?;
            println!(
                "Updated: {changed} changes + {excluded} excluded. Total: {}",
                index.entries.len(),
            );
        }
        std::thread::sleep(sleep);
    }

    println!("Watch stopped.");
    Ok(())
}

fn build_index_with_progress(drive: &str) -> Result<FileIndex> {
    let progress = Arc::new(AtomicUsize::new(0));
    let monitor_running = Arc::new(AtomicBool::new(true));
    let monitor_progress = progress.clone();
    let monitor_flag = monitor_running.clone();
    let monitor_drive = drive.to_string();
    let monitor_started = Instant::now();

    let monitor = thread::spawn(move || {
        while monitor_flag.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(2));
            if !monitor_flag.load(Ordering::SeqCst) {
                break;
            }
            let scanned = monitor_progress.load(Ordering::Relaxed);
            println!(
                "Indexing {} ... {} entries scanned (elapsed {:?})",
                monitor_drive,
                scanned,
                monitor_started.elapsed()
            );
        }
    });

    let result = FileIndex::build_with_progress(drive, Some(progress.as_ref()));
    monitor_running.store(false, Ordering::SeqCst);
    let _ = monitor.join();
    if let Ok(index) = &result {
        println!(
            "Indexing {} completed: {} entries discovered.",
            drive,
            index.entries.len()
        );
    }
    result
}

fn require_admin() -> Result<()> {
    if !is_running_as_admin() {
        return Err(anyhow!(
            "this command requires Administrator privileges to read MFT/USN Journal"
        ));
    }
    Ok(())
}

fn parse_drive_list(raw: &str) -> Result<Vec<String>> {
    let mut drives = Vec::new();
    let mut seen = HashSet::new();
    for part in raw.split(',') {
        let trimmed = part.trim();
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

fn parse_exclude_list(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    raw.split(',')
        .map(|value| value.trim().replace('/', "\\"))
        .filter(|value| !value.is_empty())
        .collect()
}

fn service_install_drives_arg(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("auto") {
        return Ok("auto".to_string());
    }

    let drives = parse_drive_list(trimmed)?;
    Ok(drives.join(","))
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

fn run_service(action: ServiceAction) -> Result<()> {
    match action {
        ServiceAction::Install {
            service_exe,
            drives,
            index,
            log_file,
            exclude,
        } => run_service_install(service_exe, drives, index, log_file, exclude),
        ServiceAction::Uninstall => run_service_uninstall(),
        ServiceAction::Status => run_service_status(),
    }
}

fn run_service_install(
    service_exe: Option<PathBuf>,
    drives_raw: String,
    index: Option<PathBuf>,
    log_file: Option<PathBuf>,
    exclude: Option<String>,
) -> Result<()> {
    if !is_running_as_admin() {
        println!("Service install requires Administrator privileges. Requesting elevation...");
        let mut args = vec![
            "service".to_string(),
            "install".to_string(),
            "--drives".to_string(),
            drives_raw.clone(),
        ];
        if let Some(path) = &service_exe {
            args.push("--service-exe".to_string());
            args.push(path.display().to_string());
        }
        if let Some(path) = &index {
            args.push("--index".to_string());
            args.push(path.display().to_string());
        }
        if let Some(path) = &log_file {
            args.push("--log-file".to_string());
            args.push(path.display().to_string());
        }
        if let Some(value) = &exclude {
            args.push("--exclude".to_string());
            args.push(value.clone());
        }
        relaunch_self_elevated(&args)?;
        return Ok(());
    }

    let drives_csv = service_install_drives_arg(&drives_raw)?;
    let service_path = resolve_service_exe_path(service_exe)?;
    let index_base = index.unwrap_or_else(default_background_index_base_path);
    let log_path = log_file.unwrap_or_else(default_background_log_path);
    let exclude_csv = exclude.unwrap_or_default();

    if let Some(parent) = index_base.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let task_command = build_service_task_command(
        &service_path,
        &drives_csv,
        &index_base,
        &log_path,
        &exclude_csv,
    );
    run_schtasks(&[
        "/create",
        "/tn",
        "Rayo Service",
        "/sc",
        "ONSTART",
        "/ru",
        "SYSTEM",
        "/rl",
        "HIGHEST",
        "/f",
        "/tr",
        &task_command,
    ])?;
    run_schtasks(&["/run", "/tn", "Rayo Service"])?;

    println!("Background task installed and started.");
    println!("Task: Rayo Service");
    println!("Service exe: {}", service_path.display());
    println!("Index base: {}", index_base.display());
    println!("Log file: {}", log_path.display());
    Ok(())
}

fn run_service_uninstall() -> Result<()> {
    if !is_running_as_admin() {
        println!("Service uninstall requires Administrator privileges. Requesting elevation...");
        let args = vec!["service".to_string(), "uninstall".to_string()];
        relaunch_self_elevated(&args)?;
        return Ok(());
    }

    let _ = run_schtasks(&["/end", "/tn", "Rayo Service"]);
    let status = Command::new("schtasks")
        .args(["/delete", "/tn", "Rayo Service", "/f"])
        .stdin(Stdio::null())
        .status()
        .context("failed to execute schtasks /delete")?;
    if status.success() {
        println!("Background task removed: Rayo Service");
    } else {
        println!("Background task was not installed or could not be removed.");
    }
    Ok(())
}

fn run_service_status() -> Result<()> {
    let output = Command::new("schtasks")
        .args(["/query", "/tn", "Rayo Service", "/fo", "LIST", "/v"])
        .output()
        .context("failed to query scheduled task status")?;
    if output.status.success() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    } else {
        println!("Rayo Service task not found.");
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            println!("{stderr}");
        }
    }
    Ok(())
}

fn run_schtasks(args: &[&str]) -> Result<()> {
    let status = Command::new("schtasks")
        .args(args)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("failed to execute schtasks with args {args:?}"))?;
    if !status.success() {
        return Err(anyhow!("schtasks failed with status {status}"));
    }
    Ok(())
}

fn relaunch_self_elevated(args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable path")?;
    let exe_quoted = escape_single_quotes(&exe.display().to_string());
    let args_literal = args
        .iter()
        .map(|arg| format!("'{}'", escape_single_quotes(arg)))
        .collect::<Vec<_>>()
        .join(", ");
    let command = format!(
        "Start-Process -FilePath '{exe_quoted}' -ArgumentList @({args_literal}) -Verb RunAs -Wait"
    );
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            command.as_str(),
        ])
        .status()
        .context("failed to invoke elevated PowerShell process")?;
    if !status.success() {
        return Err(anyhow!("elevated command failed with status {status}"));
    }
    Ok(())
}

fn escape_single_quotes(value: &str) -> String {
    value.replace('\'', "''")
}

fn build_service_task_command(
    service_exe: &Path,
    drives_csv: &str,
    index_base: &Path,
    log_path: &Path,
    exclude_csv: &str,
) -> String {
    let mut command = format!(
        "\"{}\" --drives {} --index \"{}\" --log-file \"{}\"",
        service_exe.display(),
        drives_csv,
        index_base.display(),
        log_path.display()
    );
    if !exclude_csv.trim().is_empty() {
        command.push_str(&format!(" --exclude \"{}\"", exclude_csv.replace('"', "")));
    }
    command
}

fn resolve_service_exe_path(service_exe: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = service_exe {
        if path.exists() {
            return Ok(path);
        }
        return Err(anyhow!(
            "rayo-service executable not found at {}",
            path.display()
        ));
    }

    let mut candidates = Vec::new();
    if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(local_app_data)
                .join("Rayo")
                .join("rayo-service.exe"),
        );
    }
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(base) = current_exe.parent() {
            candidates.push(base.join("rayo-service.exe"));
        }
    }
    candidates.push(Path::new("target").join("release").join("rayo-service.exe"));
    candidates.push(Path::new("target").join("debug").join("rayo-service.exe"));

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "rayo-service.exe not found. Provide --service-exe or install binaries in %LOCALAPPDATA%\\Rayo."
    ))
}

fn default_background_index_base_path() -> PathBuf {
    default_background_data_dir().join("index.rayo")
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

fn run_shell(action: ShellAction) -> Result<()> {
    match action {
        ShellAction::Install { gui_path } => install_shell_integration(gui_path),
        ShellAction::Uninstall => uninstall_shell_integration(),
        ShellAction::Doctor { gui_path } => run_shell_doctor(gui_path),
    }
}

fn install_shell_integration(gui_path: Option<PathBuf>) -> Result<()> {
    let gui_exe = resolve_gui_path(gui_path)?;
    let gui_under_command = format!("\"{}\" --under", gui_exe.display());
    let gui_open_command = format!("\"{}\" --open", gui_exe.display());
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let dir_shell = r"Software\Classes\Directory\shell\RayoSearch";
    let (dir_key, _) = hkcu.create_subkey(dir_shell)?;
    dir_key.set_value("", &"Search with Rayo here")?;
    let (dir_command_key, _) = dir_key.create_subkey("command")?;
    dir_command_key.set_value("", &format!("{gui_under_command} \"%1\""))?;

    let bg_shell = r"Software\Classes\Directory\Background\shell\RayoSearch";
    let (bg_key, _) = hkcu.create_subkey(bg_shell)?;
    bg_key.set_value("", &"Search with Rayo here")?;
    let (bg_command_key, _) = bg_key.create_subkey("command")?;
    bg_command_key.set_value("", &format!("{gui_under_command} \"%V\""))?;

    let file_shell = r"Software\Classes\*\shell\RayoSearch";
    let (file_key, _) = hkcu.create_subkey(file_shell)?;
    file_key.set_value("", &"Search with Rayo for similar files")?;
    let (file_command_key, _) = file_key.create_subkey("command")?;
    file_command_key.set_value("", &format!("{gui_open_command} \"%1\""))?;

    println!("Explorer context menu installed for current user.");
    println!("Windows 11 note: entry appears under 'Show more options'.");
    Ok(())
}

fn uninstall_shell_integration() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let dir_shell = r"Software\Classes\Directory\shell\RayoSearch";
    let bg_shell = r"Software\Classes\Directory\Background\shell\RayoSearch";
    let file_shell = r"Software\Classes\*\shell\RayoSearch";

    let _ = hkcu.delete_subkey_all(dir_shell);
    let _ = hkcu.delete_subkey_all(bg_shell);
    let _ = hkcu.delete_subkey_all(file_shell);

    println!("Explorer context menu removed for current user.");
    Ok(())
}

fn run_shell_doctor(gui_path: Option<PathBuf>) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let gui_exe = resolve_gui_path(gui_path)?;
    println!("GUI executable: {}", gui_exe.display());
    println!("GUI exists: {}", gui_exe.exists());

    let checks = [
        (
            "Directory context menu",
            r"Software\Classes\Directory\shell\RayoSearch\command",
        ),
        (
            "Directory background context menu",
            r"Software\Classes\Directory\Background\shell\RayoSearch\command",
        ),
        (
            "File context menu",
            r"Software\Classes\*\shell\RayoSearch\command",
        ),
    ];

    let mut missing = 0usize;
    for (label, key_path) in checks {
        match hkcu.open_subkey(key_path) {
            Ok(key) => {
                let command: String = key.get_value("").unwrap_or_default();
                println!("[OK] {label}: {command}");
            }
            Err(_) => {
                missing += 1;
                println!("[MISSING] {label}: {key_path}");
            }
        }
    }

    if missing > 0 {
        println!("Shell doctor: {missing} integration item(s) missing.");
    } else {
        println!("Shell doctor: all Explorer integration entries look healthy.");
    }
    Ok(())
}

fn resolve_gui_path(gui_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = gui_path {
        if path.exists() {
            return Ok(path);
        }
        return Err(anyhow!("GUI executable not found at {}", path.display()));
    }

    let current_exe = std::env::current_exe()?;
    let base_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("failed to resolve current executable directory"))?;

    let candidates = [
        base_dir.join("rayo-gui.exe"),
        Path::new("target").join("release").join("rayo-gui.exe"),
        Path::new("target").join("debug").join("rayo-gui.exe"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "rayo-gui.exe not found. Build it or pass --gui-path."
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{drive_index_path, parse_drive_list, service_install_drives_arg};

    #[test]
    fn parse_drive_list_supports_csv_and_dedup() {
        let drives = parse_drive_list("c, D:, c").expect("parse drives");
        assert_eq!(drives, vec!["C:", "D:"]);
    }

    #[test]
    fn drive_index_path_multi_drive_uses_letter_when_base_is_letter() {
        let base = Path::new("c.rayo");
        assert_eq!(drive_index_path(base, "C:", true), Path::new("c.rayo"));
        assert_eq!(drive_index_path(base, "D:", true), Path::new("d.rayo"));
    }

    #[test]
    fn service_install_drives_arg_accepts_auto_but_parse_drive_list_rejects_it() {
        let value =
            service_install_drives_arg(" auto ").expect("auto accepted for service install");
        assert_eq!(value, "auto");
        assert!(parse_drive_list("auto").is_err());
    }
}
