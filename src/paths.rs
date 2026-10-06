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

/// Pi bucket name for a working directory.
///
/// Home-relative directories encode as `-<relative>`, directories under `/tmp`
/// as `-tmp-<relative>`, everything else as `--<absolute>--`; in every case the
/// path separator becomes `-`.
pub fn encode_pi_bucket(cwd: &str) -> String {
    let path = Path::new(cwd);
    let home = home();
    if let Ok(rel) = path.strip_prefix(&home) {
        let rel = rel.to_string_lossy().replace('/', "-");
        return format!("-{rel}");
    }
    if let Ok(rel) = path.strip_prefix("/tmp") {
        let rel = rel.to_string_lossy().replace('/', "-");
        return if rel.is_empty() {
            "-tmp".to_string()
        } else {
            format!("-tmp-{rel}")
        };
    }
    let body = cwd.trim_start_matches('/').replace('/', "-");
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
    fn pi_bucket_uses_absolute_form_outside_home_and_tmp() {
        assert_eq!(encode_pi_bucket("/opt/work/x"), "--opt-work-x--");
        assert_eq!(encode_pi_bucket("/tmp"), "-tmp");
        assert_eq!(encode_pi_bucket("/tmp/scratch"), "-tmp-scratch");
    }

    #[test]
    fn pi_file_name_flattens_colons_and_dots() {
        assert_eq!(
            pi_file_name("2026-10-06T17:06:45.489Z", "abc"),
            "2026-10-06T17-06-45-489Z_abc.jsonl"
        );
    }
}
