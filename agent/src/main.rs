mod db;

use clap::{Parser, Subcommand};
use core::{FileMeta, Snapshot, compare_snapshots};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use uuid::Uuid;

use jwalk::WalkDir;
use rayon::prelude::*;
use tracing::{debug, debug_span, error, info, warn};
use tracing_appender::rolling;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about = "File Integrity Monitor", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List all baselines with a quick summary.
    Ls,
    /// Create a baseline watching the given root path and snapshot it now.
    Create { name: String, root: String },
    /// Remove a baseline (and its history). By name, or by uuid with -u.
    Remove {
        selector: String,
        /// Treat the selector as a uuid instead of a name.
        #[arg(short = 'u', long)]
        uuid: bool,
    },
    /// Check a baseline's root(s) against its stored snapshot. By name, or by uuid with -u.
    Check {
        selector: String,
        /// Treat the selector as a uuid instead of a name.
        #[arg(short = 'u', long)]
        uuid: bool,
    },
}

/// Resolve a selector to a baseline: by uuid when `by_uuid`, else by name.
fn resolve(
    db: &db::Db,
    selector: &str,
    by_uuid: bool,
) -> Result<db::Baseline, Box<dyn std::error::Error>> {
    let found = if by_uuid {
        let id = Uuid::parse_str(selector).map_err(|_| format!("invalid uuid: {selector}"))?;
        db.baseline_by_id(id.as_bytes())?
    } else {
        db.baseline_by_name(selector)?
    };
    found.ok_or_else(|| format!("no baseline found for {selector}").into())
}

/// Path to the central database (`$FIM_DB`, else `~/.fim/fim.db`).
fn fim_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("FIM_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".fim").join("fim.db")
}

/// Human-readable "time since" for a nanos-since-epoch timestamp.
fn ago(nanos: i64) -> String {
    let now = core::system_time_to_nanos(SystemTime::now());
    let secs = ((now - nanos).max(0) / 1_000_000_000) as u64;
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

fn init_logging() -> tracing_appender::non_blocking::WorkerGuard {
    let file_appender = rolling::daily(".fim", "fim.log");

    let (non_blocking_writer, guard) = tracing_appender::non_blocking(file_appender);

    let console_layer = fmt::layer().compact().with_thread_ids(true);

    let file_layer = fmt::layer()
        .with_writer(non_blocking_writer)
        .with_ansi(false)
        .with_thread_ids(true);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    guard
}

fn compute_blake3(filepath: &str, size: u64) -> io::Result<[u8; 32]> {
    let mut hasher: blake3::Hasher = blake3::Hasher::new();
    if size > 16 * 1024 * 1024 {
        hasher.update_mmap_rayon(filepath)?;
    } else {
        let file = File::open(filepath)?;
        hasher.update_reader(file)?;
    }
    Ok(*hasher.finalize().as_bytes())
}

fn build_snapshot<P: AsRef<Path>>(dir: P, exclude: &Path) -> std::io::Result<Snapshot> {
    let snapshot: Snapshot = WalkDir::new(dir)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .par_bridge()
        .filter_map(|entry| {
            let path: std::path::PathBuf = entry.path();

            let canonical_path: std::path::PathBuf = match dunce::canonicalize(&path) {
                Ok(p) => p,
                Err(e) => {
                    debug!(error = ?e, path = %path.display(), "Failed to canonicalize path");
                    return None;
                }
            };

            // Never hash our own database/logs directory.
            if canonical_path.starts_with(exclude) {
                return None;
            }

            let path_str: String = canonical_path.to_str()?.to_string();

            let _span = debug_span!("process_file", path = %path_str).entered();

            let metadata: fs::Metadata = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    debug!(error = ?e, "Failed to get metadata");
                    return None;
                }
            };
            let size: u64 = metadata.len();

            let modified: std::time::SystemTime = match metadata.modified() {
                Ok(m) => m,
                Err(e) => {
                    debug!(error = ?e, "Failed to fet modified time");
                    return None;
                }
            };

            debug!("Computing hash...");
            let hash: Option<[u8; 32]> = match compute_blake3(&path_str, size) {
                Ok(h) => Some(h),
                Err(e) => {
                    warn!(error = ?e, "Failed to compute hash, skipping hash field");
                    None
                }
            };

            Some((
                path_str,
                FileMeta {
                    size,
                    modified: core::system_time_to_nanos(modified),
                    hash,
                },
            ))
        })
        .collect();

    Ok(snapshot)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = init_logging();

    let cli: Cli = Cli::parse();

    // Open the central DB, creating its directory if needed. Its directory is
    // excluded from every scan so we never hash the DB or logs.
    let db_path = fim_db_path();
    let fim_dir = db_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&fim_dir)?;
    let exclude = dunce::canonicalize(&fim_dir).unwrap_or(fim_dir);

    let mut db = match db::Db::open(&db_path) {
        Ok(d) => d,
        Err(e) => {
            error!(error = ?e, path = %db_path.display(), "Failed to open database");
            return Err(e.into());
        }
    };

    match &cli.command {
        Command::Ls => {
            let baselines = db.list_baselines()?;
            if baselines.is_empty() {
                println!("No baselines yet. Create one: fim create <name> <path>");
            }
            for b in &baselines {
                let last = b.checked_at.map(ago).unwrap_or_else(|| "never".to_string());
                println!(
                    "{}  {:<16} files={:<7} created={:<10} last-check={:<10} roots={}",
                    b.id,
                    b.name,
                    b.file_count,
                    ago(b.created_at),
                    last,
                    b.roots.join(", "),
                );
            }
        }

        Command::Create { name, root } => {
            let canonical = dunce::canonicalize(root)?;
            let root_str = canonical.to_str().ok_or("root path is not valid UTF-8")?;
            info!(baseline = name, root = root_str, "Creating baseline...");

            let snap: Snapshot = build_snapshot(&canonical, &exclude)?;
            let id = db.upsert_baseline(name, root_str)?;
            db.save(id.as_bytes(), &snap)?;

            info!(baseline = name, id = %id, files = snap.len(), "Baseline created");
        }

        Command::Remove { selector, uuid } => {
            let baseline = resolve(&db, selector, *uuid)
                .inspect_err(|e| error!(selector = selector, error = %e, "Remove failed"))?;
            db.remove_baseline(baseline.id.as_bytes())?;
            info!(baseline = %baseline.name, id = %baseline.id, "Baseline removed");
        }

        Command::Check { selector, uuid } => {
            let baseline = resolve(&db, selector, *uuid)
                .inspect_err(|e| error!(selector = selector, error = %e, "Check failed"))?;
            info!(baseline = %baseline.name, roots = ?baseline.roots, "Checking...");

            let old: Snapshot = db.load(baseline.id.as_bytes())?;

            let mut new: Snapshot = BTreeMap::new();
            for root in &baseline.roots {
                new.extend(build_snapshot(root, &exclude)?);
            }
            let total_scanned = new.len();

            let diff: BTreeMap<String, core::Alert> = compare_snapshots(&old, &new);
            db.record_check(baseline.id.as_bytes(), total_scanned, &old, &new, &diff)?;

            if diff.is_empty() {
                info!(
                    scanned = total_scanned,
                    "Integrity check passes: No unauthorized changes."
                );
            } else {
                warn!(
                    scanned = total_scanned,
                    alerts = diff.len(),
                    "TAMPERING DETECTED!"
                );
                for (path, alert) in &diff {
                    warn!(alert_type = ?alert, file = path, "Unauthorized change")
                }
            }
        }
    }
    Ok(())
}
