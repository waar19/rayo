use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;
use rayo_core::{
    FileIndex, SearchOptions, is_running_as_admin, load_index, normalize_drive, save_index,
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
        #[arg(long, default_value_t = false)]
        dirs_only: bool,
        #[arg(long, default_value_t = false)]
        files_only: bool,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, default_value_t = false)]
        trigram: bool,
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
    /// Keep index up to date from USN Journal
    Watch {
        #[arg(long, default_value = "C")]
        drive: String,
        #[arg(long, default_value = "index.rayo")]
        index: PathBuf,
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,
    },
    /// Install or remove Explorer integration in current user registry
    Shell {
        #[command(subcommand)]
        action: ShellAction,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Index { drive, output } => run_index(&drive, output),
        Commands::Search {
            index,
            query,
            ext,
            under,
            glob,
            dirs_only,
            files_only,
            limit,
            trigram,
        } => run_search(
            index, query, ext, under, glob, dirs_only, files_only, limit, trigram,
        ),
        Commands::Content {
            query,
            under,
            ext,
            limit,
        } => run_content_search(query, under, ext, limit),
        Commands::Watch {
            drive,
            index,
            poll_ms,
        } => run_watch(&drive, index, poll_ms),
        Commands::Shell { action } => run_shell(action),
    }
}

fn run_index(drive: &str, output: PathBuf) -> Result<()> {
    require_admin()?;
    let drives = parse_drive_list(drive)?;
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
        let index = FileIndex::build(&drive)?;
        println!(
            "MFT/journal scan for {} completed in {:?}. Saving index...",
            drive,
            build_started.elapsed()
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
    dirs_only: bool,
    files_only: bool,
    limit: usize,
    trigram: bool,
) -> Result<()> {
    let mut index = load_index(&index_path)?;
    index.set_trigram_enabled(trigram);
    let started = Instant::now();
    let results = index.search(&SearchOptions {
        query,
        extension: ext,
        under_dir: under,
        glob,
        directories_only: dirs_only,
        files_only,
        limit,
        prefer_trigram: trigram,
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
    if !under.exists() {
        return Err(anyhow!("under path does not exist: {}", under.display()));
    }

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(true)
        .build(&query)
        .map_err(|err| anyhow!("invalid regex query: {err}"))?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();
    let mut outputs = Vec::new();
    let started = Instant::now();
    let ext_filter = ext.map(|value| value.trim_start_matches('.').to_ascii_lowercase());

    let walker = WalkBuilder::new(&under).standard_filters(true).build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Some(required_ext) = &ext_filter {
            let file_ext = path
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| value.to_ascii_lowercase());
            if file_ext.as_deref() != Some(required_ext.as_str()) {
                continue;
            }
        }
        if outputs.len() >= limit {
            break;
        }

        let display_path = path.display().to_string();
        if let Err(err) = searcher.search_path(
            &matcher,
            path,
            UTF8(|line_number, line| {
                if outputs.len() >= limit {
                    return Ok(false);
                }
                outputs.push(format!(
                    "{}:{}:{}",
                    display_path,
                    line_number,
                    line.trim_end()
                ));
                Ok(true)
            }),
        ) {
            let err_text = err.to_string();
            if err_text.contains("invalid utf-8 sequence") {
                continue;
            }
            return Err(anyhow!(
                "content search failed on {}: {err}",
                path.display()
            ));
        }
    }

    for line in &outputs {
        println!("{line}");
    }
    println!(
        "Content results: {} in {:?}",
        outputs.len(),
        started.elapsed()
    );
    Ok(())
}

fn run_watch(drive: &str, index_path: PathBuf, poll_ms: u64) -> Result<()> {
    require_admin()?;
    let drive = normalize_drive(drive)?;
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
            FileIndex::build(&drive)?
        }
    } else {
        println!(
            "No index found at {}. Building initial index...",
            index_path.display()
        );
        FileIndex::build(&drive)?
    };
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
        if changed > 0 {
            save_index(&index, &index_path)?;
            println!(
                "Updated: {changed} changes applied. Total: {}",
                index.entries.len()
            );
        }
        std::thread::sleep(sleep);
    }

    println!("Watch stopped.");
    Ok(())
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

    use super::{drive_index_path, parse_drive_list};

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
}
