//! `dshpi` — convert sessions between DeepSeek Harness and Oh My Pi.
//!
//! Running the binary with no subcommand opens the Ratatui browser; the
//! subcommands expose the same conversions for scripts.

mod convert;
mod dsh;
mod paths;
mod pi;
mod transcript;
mod tui;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

/// Which harness a session belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Side {
    /// DeepSeek Harness.
    Dsh,
    /// Oh My Pi.
    Pi,
}

/// Readable export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportFormat {
    /// Markdown document.
    Md,
    /// Plain text.
    Text,
}

#[derive(Debug, Parser)]
#[command(
    name = "dshpi",
    version,
    about = "Convert sessions between DeepSeek Harness (DSH) and Oh My Pi (OMP)",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the interactive browser (default).
    Tui,
    /// List discovered sessions.
    List {
        /// Which harness to list; both when omitted.
        #[arg(value_enum)]
        side: Option<Side>,
        /// Keep only sessions whose title, cwd, or id contains this text.
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Convert one session to the other harness.
    Convert {
        /// Session id, id prefix, or path to a session file.
        session: String,
        /// Destination harness; inferred from the source when omitted.
        #[arg(long, value_enum)]
        to: Option<Side>,
        /// Write under this directory instead of the destination session root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Report the destination without writing.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite an existing destination file.
        #[arg(long)]
        force: bool,
        /// Also convert sessions that hold no conversation.
        #[arg(long)]
        include_empty: bool,
    },
    /// Convert every session of one harness.
    ConvertAll {
        /// Source harness.
        #[arg(value_enum)]
        from: Side,
        /// Keep only sessions whose title, cwd, or id contains this text.
        #[arg(short, long)]
        filter: Option<String>,
        /// Write under this directory instead of the destination session root.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Report destinations without writing.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite existing destination files.
        #[arg(long)]
        force: bool,
        /// Also convert sessions that hold no conversation.
        #[arg(long)]
        include_empty: bool,
    },
    /// Delete migrated sessions that hold no conversation.
    PruneEmpty {
        /// Keep only sessions whose title, cwd, or id contains this text.
        #[arg(short, long)]
        filter: Option<String>,
        /// Delete the listed files; without it the command only reports them.
        #[arg(long)]
        execute: bool,
    },
    /// Convert every session out and back, reporting any row that changed.
    Verify {
        /// Which harness to check; both when omitted.
        #[arg(value_enum)]
        side: Option<Side>,
        /// Keep only sessions whose title, cwd, or id contains this text.
        #[arg(short, long)]
        filter: Option<String>,
        /// Print every differing row instead of the first few.
        #[arg(long)]
        all: bool,
    },
    /// Write one session as a readable document.
    Export {
        /// Session id, id prefix, or path to a session file.
        session: String,
        /// Output format.
        #[arg(long, value_enum, default_value = "md")]
        format: ExportFormat,
        /// Output file; standard output when omitted.
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => tui::run(),
        Command::List { side, filter } => list(side, filter.as_deref()),
        Command::Convert {
            session,
            to,
            out,
            dry_run,
            force,
            include_empty,
        } => {
            let located = locate(&session)?;
            let to = to.unwrap_or(match located.side {
                Side::Dsh => Side::Pi,
                Side::Pi => Side::Dsh,
            });
            if to == located.side {
                bail!("source and destination are both {:?}", to);
            }
            let outcome = convert_one(&located, out.as_deref(), dry_run, force, include_empty)?;
            println!("{outcome}");
            Ok(())
        }
        Command::ConvertAll {
            from,
            filter,
            out,
            dry_run,
            force,
            include_empty,
        } => convert_all(
            from,
            filter.as_deref(),
            out.as_deref(),
            dry_run,
            force,
            include_empty,
        ),
        Command::PruneEmpty { filter, execute } => prune_empty(filter.as_deref(), execute),
        Command::Verify { side, filter, all } => verify(side, filter.as_deref(), all),
        Command::Export {
            session,
            format,
            out,
        } => export(&session, format, out.as_deref()),
    }
}

/// A session resolved from a user-supplied id or path.
pub struct Located {
    /// Harness the session belongs to.
    pub side: Side,
    /// Session file.
    pub path: PathBuf,
    /// Session identifier.
    pub id: String,
    /// Working directory recorded in the header.
    pub cwd: String,
}

/// Resolves a session argument to a concrete file.
pub fn locate(needle: &str) -> Result<Located> {
    let as_path = Path::new(needle);
    if as_path.is_file() {
        return classify(as_path);
    }
    let lower = needle.to_lowercase();
    for found in dsh::discover(&paths::dsh_root()) {
        if found.id.to_lowercase().starts_with(&lower) {
            return Ok(Located {
                side: Side::Dsh,
                path: found.path,
                id: found.id,
                cwd: found.cwd,
            });
        }
    }
    for found in pi::discover(&paths::pi_root()) {
        if found.id.to_lowercase().starts_with(&lower) {
            return Ok(Located {
                side: Side::Pi,
                path: found.path,
                id: found.id,
                cwd: found.cwd,
            });
        }
    }
    Err(anyhow!("no session matches {needle:?}"))
}

/// Identifies which harness wrote a session file.
fn classify(path: &Path) -> Result<Located> {
    if path.to_string_lossy().ends_with(".jsonl.zstd") {
        let header = dsh::read_header(path)?;
        return Ok(Located {
            side: Side::Dsh,
            path: path.to_path_buf(),
            id: header.id,
            cwd: header.cwd,
        });
    }
    let file = pi::read_file(path)?;
    Ok(Located {
        side: Side::Pi,
        path: path.to_path_buf(),
        id: file.id().to_string(),
        cwd: file.cwd().to_string(),
    })
}

/// Destination path a conversion of `located` would write.
pub fn destination(located: &Located, out: Option<&Path>) -> Result<PathBuf> {
    match located.side {
        Side::Dsh => {
            let header = dsh::read_header(&located.path)?;
            let root = out
                .map(Path::to_path_buf)
                .unwrap_or_else(paths::pi_root);
            let name = paths::pi_file_name(&convert::iso(header.created_at), &header.id);
            Ok(root.join(paths::encode_pi_bucket(&header.cwd)).join(name))
        }
        Side::Pi => {
            let file = pi::read_file(&located.path)?;
            let root = out
                .map(Path::to_path_buf)
                .unwrap_or_else(paths::dsh_root);
            Ok(root
                .join(paths::encode_dsh_bucket(file.cwd()))
                .join(file.id())
                .join(format!("session.v{}.jsonl.zstd", dsh::CURRENT_GENERATION)))
        }
    }
}

/// What one conversion did.
pub enum Outcome {
    /// The destination was written, or would be under `--dry-run`.
    Written(String),
    /// The source held no conversation and was left alone.
    Skipped(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Written(line) => write!(out, "{line}"),
            Outcome::Skipped(line) => write!(out, "{line}"),
        }
    }
}

impl Outcome {
    /// True when the destination was written.
    pub fn wrote(&self) -> bool {
        matches!(self, Outcome::Written(_))
    }
}

/// Converts one session and reports what happened.
///
/// A session that holds no conversation is skipped unless `include_empty` is
/// set: cancelled and never-used launches leave behind logs of nothing but
/// startup records, and importing them buries the real sessions.
pub fn convert_one(
    located: &Located,
    out: Option<&Path>,
    dry_run: bool,
    force: bool,
    include_empty: bool,
) -> Result<Outcome> {
    let target = destination(located, out)?;
    let prefix = if dry_run { "[dry-run] " } else { "" };

    match located.side {
        Side::Dsh => {
            let log = dsh::read_log(&located.path)?;
            let (file, report) = convert::dsh_to_pi(&log, &located.path);
            if !include_empty && file.message_count() == 0 {
                return Ok(Outcome::Skipped(format!(
                    "{} skipped: no conversation",
                    located.id
                )));
            }
            guard_destination(&target, force, dry_run)?;
            if !dry_run {
                pi::write_file(&target, &file)?;
            }
            Ok(Outcome::Written(format!(
                "{prefix}{} -> {} ({} entries: {} mapped, {} carried)",
                located.id,
                target.display(),
                report.rows,
                report.mapped,
                report.passthrough
            )))
        }
        Side::Pi => {
            let file = pi::read_file(&located.path)?;
            let (log, report) = convert::pi_to_dsh(&file, false);
            if !include_empty && log.message_count() == 0 {
                return Ok(Outcome::Skipped(format!(
                    "{} skipped: no conversation",
                    located.id
                )));
            }
            guard_destination(&target, force, dry_run)?;
            if !dry_run {
                dsh::write_log(&target, &log, 10)?;
            }
            Ok(Outcome::Written(format!(
                "{prefix}{} -> {} ({} events: {} mapped, {} restored)",
                located.id,
                target.display(),
                report.rows,
                report.mapped,
                report.passthrough
            )))
        }
    }
}

/// Refuses to clobber an existing destination unless asked to.
fn guard_destination(target: &Path, force: bool, dry_run: bool) -> Result<()> {
    if target.exists() && !force && !dry_run {
        bail!("{} already exists; pass --force to overwrite", target.display());
    }
    Ok(())
}

/// Deletes migrated Pi sessions that hold no conversation.
///
/// Only sessions this tool wrote are candidates, so a native Pi session is
/// never removed even when it is empty.
fn prune_empty(filter: Option<&str>, execute: bool) -> Result<()> {
    let needle = filter.map(str::to_lowercase);
    let keep = |fields: &[&str]| match &needle {
        None => true,
        Some(needle) => fields.iter().any(|f| f.to_lowercase().contains(needle)),
    };

    let mut removed = 0usize;
    let mut bytes = 0u64;
    for found in pi::discover(&paths::pi_root()) {
        if !found.migrated || found.messages > 0 {
            continue;
        }
        if !keep(&[&found.id, &found.cwd, &found.title]) {
            continue;
        }
        // Re-read before deleting: discovery streams, this confirms.
        let file = pi::read_file(&found.path)?;
        if !file.is_migrated() || file.message_count() > 0 {
            continue;
        }
        println!(
            "{} {} ({} entries, {})",
            if execute { "removed" } else { "[dry-run]" },
            found.path.display(),
            found.entries,
            found.size
        );
        removed += 1;
        bytes += found.size;
        if execute {
            std::fs::remove_file(&found.path)
                .with_context(|| format!("remove {}", found.path.display()))?;
            if let Some(parent) = found.path.parent() {
                // Succeeds only when the directory is now empty.
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    println!(
        "{} empty migrated session(s), {} bytes{}",
        removed,
        bytes,
        if execute { " removed" } else { "; pass --execute to delete" }
    );
    Ok(())
}

/// Lists sessions on one or both sides.
fn list(side: Option<Side>, filter: Option<&str>) -> Result<()> {
    let needle = filter.map(str::to_lowercase);
    let matches = |haystack: &[&str]| match &needle {
        None => true,
        Some(needle) => haystack
            .iter()
            .any(|field| field.to_lowercase().contains(needle)),
    };

    if side != Some(Side::Pi) {
        println!("# DSH  ({})", paths::dsh_root().display());
        for found in dsh::discover(&paths::dsh_root()) {
            let title = found.title.clone().unwrap_or_default();
            if !matches(&[&found.id, &found.cwd, &title]) {
                continue;
            }
            println!(
                "{:<38} v{} {:>6} evt {:>5} msg  {:<28} {}",
                found.id,
                found.generation,
                found.events,
                found.messages,
                shorten(&found.cwd, 28),
                title
            );
        }
    }
    if side != Some(Side::Dsh) {
        println!("# Pi   ({})", paths::pi_root().display());
        for found in pi::discover(&paths::pi_root()) {
            if !matches(&[&found.id, &found.cwd, &found.title]) {
                continue;
            }
            println!(
                "{:<38}    {:>6} evt {:>5} msg  {:<28} {}",
                found.id,
                found.entries,
                found.messages,
                shorten(&found.cwd, 28),
                found.title
            );
        }
    }
    Ok(())
}

/// Converts every session of one harness.
fn convert_all(
    from: Side,
    filter: Option<&str>,
    out: Option<&Path>,
    dry_run: bool,
    force: bool,
    include_empty: bool,
) -> Result<()> {
    let needle = filter.map(str::to_lowercase);
    let keep = |fields: &[&str]| match &needle {
        None => true,
        Some(needle) => fields.iter().any(|f| f.to_lowercase().contains(needle)),
    };

    let targets: Vec<Located> = match from {
        Side::Dsh => dsh::discover(&paths::dsh_root())
            .into_iter()
            .filter(|found| {
                let title = found.title.clone().unwrap_or_default();
                keep(&[&found.id, &found.cwd, &title])
            })
            .map(|found| Located {
                side: Side::Dsh,
                path: found.path,
                id: found.id,
                cwd: found.cwd,
            })
            .collect(),
        Side::Pi => pi::discover(&paths::pi_root())
            .into_iter()
            .filter(|found| keep(&[&found.id, &found.cwd, &found.title]))
            .map(|found| Located {
                side: Side::Pi,
                path: found.path,
                id: found.id,
                cwd: found.cwd,
            })
            .collect(),
    };

    let mut failures = 0usize;
    let mut skipped = 0usize;
    for located in &targets {
        match convert_one(located, out, dry_run, force, include_empty) {
            Ok(outcome) => {
                if !outcome.wrote() {
                    skipped += 1;
                }
                println!("{outcome}");
            }
            Err(error) => {
                failures += 1;
                eprintln!("{}: {error:#}", located.id);
            }
        }
    }
    println!(
        "{} session(s), {} converted, {skipped} empty, {failures} failed",
        targets.len(),
        targets.len() - skipped - failures
    );
    if failures > 0 {
        bail!("{failures} conversion(s) failed");
    }
    Ok(())
}

/// Converts every session out and back, reporting rows that did not survive.
///
/// A DSH session is converted to Pi and back; a Pi session is converted to DSH
/// and back. Both directions must reproduce every source row byte for byte.
fn verify(side: Option<Side>, filter: Option<&str>, show_all: bool) -> Result<()> {
    let needle = filter.map(str::to_lowercase);
    let keep = |fields: &[&str]| match &needle {
        None => true,
        Some(needle) => fields.iter().any(|f| f.to_lowercase().contains(needle)),
    };

    let mut checked = 0usize;
    let mut perfect = 0usize;
    let mut damaged = 0usize;
    let mut unreadable = 0usize;

    if side != Some(Side::Pi) {
        for found in dsh::discover(&paths::dsh_root()) {
            let title = found.title.clone().unwrap_or_default();
            if !keep(&[&found.id, &found.cwd, &title]) {
                continue;
            }
            checked += 1;
            let log = match dsh::read_log(&found.path) {
                Ok(log) => log,
                Err(error) => {
                    unreadable += 1;
                    eprintln!("{} unreadable: {error:#}", found.id);
                    continue;
                }
            };
            let (file, _) = convert::dsh_to_pi(&log, &found.path);
            let (back, _) = convert::pi_to_dsh(&file, false);
            let differences = diff_dsh(&log, &back);
            if differences.is_empty() {
                perfect += 1;
            } else {
                damaged += 1;
                println!("{} v{}: {} row(s) differ", found.id, found.generation, differences.len());
                for line in differences.iter().take(if show_all { usize::MAX } else { 3 }) {
                    println!("    {line}");
                }
            }
        }
    }

    if side != Some(Side::Dsh) {
        for found in pi::discover(&paths::pi_root()) {
            if !keep(&[&found.id, &found.cwd, &found.title]) {
                continue;
            }
            checked += 1;
            let file = match pi::read_file(&found.path) {
                Ok(file) => file,
                Err(error) => {
                    unreadable += 1;
                    eprintln!("{} unreadable: {error:#}", found.id);
                    continue;
                }
            };
            let (log, _) = convert::pi_to_dsh(&file, false);
            let (back, _) = convert::dsh_to_pi(&log, &found.path);
            let differences = diff_pi(&file, &back);
            if differences.is_empty() {
                perfect += 1;
            } else {
                damaged += 1;
                println!("{}: {} difference(s)", found.id, differences.len());
                for line in differences.iter().take(if show_all { usize::MAX } else { 3 }) {
                    println!("    {line}");
                }
            }
        }
    }

    println!("{checked} checked · {perfect} identical · {damaged} changed · {unreadable} unreadable");
    if damaged > 0 || unreadable > 0 {
        bail!("{damaged} session(s) did not round-trip, {unreadable} unreadable");
    }
    Ok(())
}

/// Describes every DSH row that differs between an original and a round trip.
fn diff_dsh(before: &dsh::Log, after: &dsh::Log) -> Vec<String> {
    let mut out = Vec::new();
    if before.header.raw != after.header.raw {
        out.push("header".to_string());
    }
    if before.events.len() != after.events.len() {
        out.push(format!(
            "row count {} -> {}",
            before.events.len(),
            after.events.len()
        ));
    }
    for (index, (a, b)) in before.events.iter().zip(after.events.iter()).enumerate() {
        if a.raw != b.raw {
            out.push(format!("row {index} ({})", a.kind));
        }
    }
    out
}

/// Describes every readable difference between an original Pi session and a round trip.
///
/// A Pi session projected onto DSH gains the turn and step scaffolding DSH
/// replay requires, so entry counts legitimately grow. What must survive intact
/// is the header, the title, and every transcript block.
fn diff_pi(before: &pi::SessionFile, after: &pi::SessionFile) -> Vec<String> {
    let mut out = Vec::new();
    if before.header != after.header {
        out.push("header".to_string());
    }
    if before.title != after.title {
        out.push(format!("title {:?} -> {:?}", before.title, after.title));
    }
    let left = transcript::from_pi(before);
    let right = transcript::from_pi(after);
    if left.len() != right.len() {
        out.push(format!(
            "transcript blocks {} -> {}",
            left.len(),
            right.len()
        ));
    }
    for (index, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        if a.speaker != b.speaker || a.heading != b.heading || a.body != b.body {
            out.push(format!("block {index} ({})", a.speaker.label()));
        }
    }
    out
}

/// Writes one session as a readable document.
fn export(session: &str, format: ExportFormat, out: Option<&Path>) -> Result<()> {
    let located = locate(session)?;
    let (title, blocks) = match located.side {
        Side::Dsh => {
            let log = dsh::read_log(&located.path)?;
            (log.title().unwrap_or_default(), transcript::from_dsh(&log))
        }
        Side::Pi => {
            let file = pi::read_file(&located.path)?;
            (file.title.clone(), transcript::from_pi(&file))
        }
    };
    let rendered = match format {
        ExportFormat::Md => transcript::to_markdown(&title, &located.cwd, &blocks),
        ExportFormat::Text => blocks
            .iter()
            .map(|block| {
                format!(
                    "[{}] {}{}\n{}\n",
                    block.speaker.label(),
                    block.heading,
                    if block.heading.is_empty() { "" } else { " " },
                    block.body
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    match out {
        Some(path) => {
            std::fs::write(path, rendered)
                .with_context(|| format!("write {}", path.display()))?;
            println!("wrote {}", path.display());
        }
        None => print!("{rendered}"),
    }
    Ok(())
}

/// Truncates a path for fixed-width listing columns.
pub fn shorten(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    let tail: String = chars[chars.len() - (width - 1)..].iter().collect();
    format!("…{tail}")
}
