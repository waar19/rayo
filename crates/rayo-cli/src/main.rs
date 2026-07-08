use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
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
        } => run_search(index, query, ext, under, glob, dirs_only, files_only, limit),
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
    let drive = normalize_drive(drive)?;
    let started = Instant::now();
    println!(
        "Starting index build on {}. This can take several minutes...",
        drive
    );
    let build_started = Instant::now();
    let index = FileIndex::build(&drive)?;
    println!(
        "MFT/journal scan completed in {:?}. Saving index...",
        build_started.elapsed()
    );
    let save_started = Instant::now();
    save_index(&index, &output)?;
    println!("Index persisted in {:?}", save_started.elapsed());
    println!(
        "Index generated: {} entries in {:?} -> {}",
        index.entries.len(),
        started.elapsed(),
        output.display()
    );
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
) -> Result<()> {
    let index = load_index(&index_path)?;
    let started = Instant::now();
    let results = index.search(&SearchOptions {
        query,
        extension: ext,
        under_dir: under,
        glob,
        directories_only: dirs_only,
        files_only,
        limit,
    });

    for result in &results {
        let entry_type = if result.is_directory { "DIR " } else { "FILE" };
        println!("[{entry_type}] {}", result.path);
    }
    println!("Results: {} in {:?}", results.len(), started.elapsed());
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

fn run_shell(action: ShellAction) -> Result<()> {
    match action {
        ShellAction::Install { gui_path } => install_shell_integration(gui_path),
        ShellAction::Uninstall => uninstall_shell_integration(),
    }
}

fn install_shell_integration(gui_path: Option<PathBuf>) -> Result<()> {
    let gui_exe = resolve_gui_path(gui_path)?;
    let gui_command = format!("\"{}\" --under", gui_exe.display());
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);

    let dir_shell =
        r"Software\Classes\Directory\shell\RayoSearch";
    let (dir_key, _) = hkcu.create_subkey(dir_shell)?;
    dir_key.set_value("", &"Search with Rayo here")?;
    let (dir_command_key, _) = dir_key.create_subkey("command")?;
    dir_command_key.set_value("", &format!("{gui_command} \"%1\""))?;

    let bg_shell =
        r"Software\Classes\Directory\Background\shell\RayoSearch";
    let (bg_key, _) = hkcu.create_subkey(bg_shell)?;
    bg_key.set_value("", &"Search with Rayo here")?;
    let (bg_command_key, _) = bg_key.create_subkey("command")?;
    bg_command_key.set_value("", &format!("{gui_command} \"%V\""))?;

    println!("Explorer context menu installed for current user.");
    println!("Windows 11 note: entry appears under 'Show more options'.");
    Ok(())
}

fn uninstall_shell_integration() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let dir_shell =
        r"Software\Classes\Directory\shell\RayoSearch";
    let bg_shell =
        r"Software\Classes\Directory\Background\shell\RayoSearch";

    let _ = hkcu.delete_subkey_all(dir_shell);
    let _ = hkcu.delete_subkey_all(bg_shell);

    println!("Explorer context menu removed for current user.");
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
