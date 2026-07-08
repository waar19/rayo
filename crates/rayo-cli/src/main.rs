use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use rayo_core::{
    FileIndex, SearchOptions, is_running_as_admin, load_index, normalize_drive, save_index,
};

#[derive(Parser, Debug)]
#[command(author, version, about = "Buscador NTFS ultrarrapido para Windows")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Construye o reconstruye el indice desde MFT/USN
    Index {
        #[arg(long, default_value = "C")]
        drive: String,
        #[arg(long, default_value = "index.rayo")]
        output: PathBuf,
    },
    /// Busca en un indice existente
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
    /// Mantiene el indice actualizado con el USN Journal
    Watch {
        #[arg(long, default_value = "C")]
        drive: String,
        #[arg(long, default_value = "index.rayo")]
        index: PathBuf,
        #[arg(long, default_value_t = 500)]
        poll_ms: u64,
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
        } => run_search(index, query, ext, under, glob, dirs_only, files_only, limit),
        Commands::Watch {
            drive,
            index,
            poll_ms,
        } => run_watch(&drive, index, poll_ms),
    }
}

fn run_index(drive: &str, output: PathBuf) -> Result<()> {
    require_admin()?;
    let drive = normalize_drive(drive)?;
    let started = Instant::now();
    let index = FileIndex::build(&drive)?;
    save_index(&index, &output)?;
    println!(
        "Indice generado: {} entradas en {:?} -> {}",
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
    println!("Resultados: {} en {:?}", results.len(), started.elapsed());
    Ok(())
}

fn run_watch(drive: &str, index_path: PathBuf, poll_ms: u64) -> Result<()> {
    require_admin()?;
    let drive = normalize_drive(drive)?;
    let mut index = if index_path.exists() {
        let loaded = load_index(&index_path)?;
        if loaded.drive.eq_ignore_ascii_case(&drive) {
            loaded
        } else {
            FileIndex::build(&drive)?
        }
    } else {
        FileIndex::build(&drive)?
    };
    save_index(&index, &index_path)?;
    println!(
        "Watch iniciado en {} ({} entradas). Ctrl+C para salir.",
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
                "Actualizado: {changed} cambios aplicados. Total: {}",
                index.entries.len()
            );
        }
        std::thread::sleep(sleep);
    }

    println!("Watch finalizado.");
    Ok(())
}

fn require_admin() -> Result<()> {
    if !is_running_as_admin() {
        return Err(anyhow!(
            "este comando requiere permisos de Administrador para leer MFT/USN Journal"
        ));
    }
    Ok(())
}
