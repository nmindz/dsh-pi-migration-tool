//! On-disk layout of the harnesses: session roots and the cwd-bucket encodings
//! each one uses for its per-project session directory.
//!
//! DSH:    `<dsh-home>/sessions/<bucket>/<session-id>/session.v<N>.jsonl.zstd`
//! Pi:     `<omp-home>/agent/sessions/<bucket>/<timestamp>_<session-id>.jsonl`
//! Claude: `<claude-config>/projects/<bucket>/<session-id>.jsonl`

use std::path::{Path, PathBuf};

/// Home directory, or `/` when the platform cannot report one.
pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
}

/// Root holding DSH per-project session buckets. `DSH_HOME` overrides `~/.dsh`.
pub fn dsh_root() -> PathBuf {
    match std::env::var_os("DSH_SESSIONS_DIR") {
        Some(v) => PathBuf::from(v),
        None => match std::env::var_os("DSH_HOME") {
            Some(v) => PathBuf::from(v).join("sessions"),
            None => home().join(".dsh").join("sessions"),
        },
    }
}

/// Root holding Pi per-project session buckets. `OMP_HOME` overrides `~/.omp`.
pub fn pi_root() -> PathBuf {
    match std::env::var_os("OMP_SESSIONS_DIR") {
        Some(v) => PathBuf::from(v),
        None => match std::env::var_os("OMP_HOME") {
            Some(v) => PathBuf::from(v).join("agent").join("sessions"),
            None => home().join(".omp").join("agent").join("sessions"),
        },
    }
}

/// Claude Code's configuration directory. `CLAUDE_CONFIG_DIR` overrides
/// `~/.claude`, exactly as it does for Claude Code itself.
fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from)
}

/// Root holding Claude Code per-project transcripts.
pub fn claude_root() -> PathBuf {
    match std::env::var_os("CLAUDE_SESSIONS_DIR") {
        Some(v) => PathBuf::from(v),
        None => claude_config_dir()
            .unwrap_or_else(|| home().join(".claude"))
            .join("projects"),
    }
}

/// Claude Code's global settings file, which registers every project it opened.
pub fn claude_settings_file() -> PathBuf {
    match claude_config_dir() {
        Some(dir) => dir.join(".claude.json"),
        None => home().join(".claude.json"),
    }
}

/// Claude Code bucket name for a working directory: every character that is
/// not an ASCII letter or digit becomes `-`, so the encoding cannot be undone.
pub fn encode_claude_bucket(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// DSH bucket name for a working directory: `--Users-me-Projects-x--`.
pub fn encode_dsh_bucket(cwd: &str) -> String {
    let body = cwd.trim_start_matches('/').replace('/', "-");
    format!("--{body}--")
}

/// Pi bucket name for a working directory, exactly as OMP derives it.
///
/// OMP compares canonical paths (symlinks resolved), checks its temp root
/// first, then home: a directory under the temp root encodes as
/// `-tmp-<relative>`, one under home as `-<relative>`, anything else as
/// `--<absolute>--`. A bucket that disagrees is moved by OMP on first access,
/// so the files would not stay where this tool reported them.
pub fn encode_pi_bucket(cwd: &str) -> String {
    pi_bucket(
        &canonical(Path::new(cwd)),
        &canonical(&home()),
        &canonical(&temp_root()),
    )
}

/// The path with symlinks resolved when it exists, else unchanged.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The temp root OMP uses, Node's `os.tmpdir()`: `$TMPDIR`, `$TMP`, or
/// `$TEMP`, else `/tmp`. On macOS that is the per-user `/var/folders/…/T`,
/// not `/tmp`.
fn temp_root() -> PathBuf {
    ["TMPDIR", "TMP", "TEMP"]
        .into_iter()
        .find_map(|key| std::env::var_os(key).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// [`encode_pi_bucket`] over already canonical paths.
fn pi_bucket(cwd: &Path, home: &Path, temp: &Path) -> String {
    let flatten = |path: &Path| path.to_string_lossy().replace(['/', '\\', ':'], "-");
    if let Ok(rel) = cwd.strip_prefix(temp) {
        let rel = flatten(rel);
        return if rel.is_empty() { "-tmp".to_string() } else { format!("-tmp-{rel}") };
    }
    if let Ok(rel) = cwd.strip_prefix(home) {
        return format!("-{}", flatten(rel));
    }
    let absolute = cwd.to_string_lossy();
    let body = absolute.strip_prefix('/').unwrap_or(&absolute).replace(['/', '\\', ':'], "-");
    format!("--{body}--")
}

/// Pi session file name: `<iso-timestamp-with-dashes>_<session-id>.jsonl`.
pub fn pi_file_name(iso_timestamp: &str, session_id: &str) -> String {
    let stamp = iso_timestamp.replace(':', "-").replace('.', "-");
    format!("{stamp}_{session_id}.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsh_bucket_wraps_the_absolute_path() {
        assert_eq!(encode_dsh_bucket("/Users/me/Projects/x"), "--Users-me-Projects-x--");
        assert_eq!(encode_dsh_bucket("/Users/me"), "--Users-me--");
    }

    #[test]
    fn claude_bucket_flattens_every_non_alphanumeric_character() {
        assert_eq!(
            encode_claude_bucket("/Users/me.x/Projects/_drafts/a-b"),
            "-Users-me-x-Projects--drafts-a-b"
        );
    }

    #[test]
    fn pi_bucket_checks_the_temp_root_before_home_and_falls_back_to_absolute() {
        let (home, tmp) = (Path::new("/home/me"), Path::new("/tmp"));
        assert_eq!(pi_bucket(Path::new("/home/me/Projects/x"), home, tmp), "-Projects-x");
        assert_eq!(pi_bucket(Path::new("/home/me"), home, tmp), "-");
        assert_eq!(pi_bucket(Path::new("/opt/work/x"), home, tmp), "--opt-work-x--");
        assert_eq!(pi_bucket(Path::new("/tmp"), home, tmp), "-tmp");
        assert_eq!(pi_bucket(Path::new("/tmp/scratch"), home, tmp), "-tmp-scratch");
        assert_eq!(pi_bucket(Path::new("/opt/a:b"), home, tmp), "--opt-a-b--");
    }

    #[test]
    fn pi_bucket_follows_the_macos_temp_root_not_slash_tmp() {
        let home = Path::new("/Users/me");
        let tmp = Path::new("/private/var/folders/2m/xyz/T");
        assert_eq!(pi_bucket(Path::new("/private/var/folders/2m/xyz/T"), home, tmp), "-tmp");
        assert_eq!(pi_bucket(Path::new("/private/var/folders/2m/xyz/T/run"), home, tmp), "-tmp-run");
        assert_eq!(pi_bucket(Path::new("/private/tmp/scratch"), home, tmp), "--private-tmp-scratch--");
    }

    #[test]
    fn pi_file_name_flattens_colons_and_dots() {
        assert_eq!(
            pi_file_name("2026-10-06T17:06:45.489Z", "abc"),
            "2026-10-06T17-06-45-489Z_abc.jsonl"
        );
    }
}
