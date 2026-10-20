//! Where Quarry keeps its few settings files, with a **portable mode**.
//!
//! Portable mode is on when a `QuarryData` folder (or a `quarry.portable`
//! marker file) sits next to the executable. Then everything — settings,
//! recent files, connection profiles, custom collations — lives in that
//! folder, nothing is written to the user profile or registry, and recent
//! file paths on the same drive are stored *relative to the executable* so
//! a USB stick works no matter which drive letter it gets.
//!
//! Otherwise the per-user config directory is used:
//! Windows `%APPDATA%\Quarry`, macOS `~/Library/Application Support/Quarry`,
//! Linux `$XDG_CONFIG_HOME/quarry` (or `~/.config/quarry`).
//! `QUARRY_DATA_DIR` overrides both.

use std::path::{Path, PathBuf};

pub const PORTABLE_DIR: &str = "QuarryData";
pub const PORTABLE_MARKER: &str = "quarry.portable";

pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.canonicalize().ok()?.parent().map(Path::to_path_buf)
}

pub fn is_portable() -> bool {
    exe_dir().is_some_and(|d| d.join(PORTABLE_DIR).is_dir() || d.join(PORTABLE_MARKER).is_file())
}

fn os_config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        env("APPDATA").map(|p| p.join("Quarry"))
    } else if cfg!(target_os = "macos") {
        env("HOME").map(|h| h.join("Library/Application Support/Quarry"))
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))).map(|p| p.join("quarry"))
    }
}

pub fn data_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("QUARRY_DATA_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(d));
    }
    if is_portable() {
        return exe_dir().map(|d| d.join(PORTABLE_DIR));
    }
    os_config_dir()
}

pub fn read(name: &str) -> Option<String> {
    std::fs::read_to_string(data_dir()?.join(name)).ok()
}

pub fn write(name: &str, content: &str) -> Result<(), String> {
    let dir = data_dir().ok_or("No settings directory available")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // write-then-rename so a crash never leaves a half-written file
    let tmp = dir.join(format!("{name}.tmp"));
    std::fs::write(&tmp, content).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, dir.join(name)).map_err(|e| e.to_string())
}

/// Switch to portable mode: create `QuarryData` beside the executable and
/// copy the current settings into it.
pub fn enable_portable() -> Result<PathBuf, String> {
    let exe = exe_dir().ok_or("Can't locate the executable")?;
    let target = exe.join(PORTABLE_DIR);
    let source = os_config_dir();
    std::fs::create_dir_all(&target).map_err(|e| format!("Can't create {} ({e}). Is the folder writable?", target.display()))?;
    if let Some(src) = source.filter(|s| s.is_dir()) {
        for f in ["settings.tsv", "recent.txt", "connections.tsv", "collations.tsv"] {
            let (a, b) = (src.join(f), target.join(f));
            if a.is_file() && !b.exists() {
                let _ = std::fs::copy(a, b);
            }
        }
    }
    Ok(target)
}

/// Leave portable mode (the QuarryData folder is kept, just renamed aside).
pub fn disable_portable() -> Result<(), String> {
    let exe = exe_dir().ok_or("Can't locate the executable")?;
    let _ = std::fs::remove_file(exe.join(PORTABLE_MARKER));
    let d = exe.join(PORTABLE_DIR);
    if d.is_dir() {
        std::fs::rename(&d, exe.join(format!("{PORTABLE_DIR}.disabled"))).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Recent files (relative to the exe in portable mode)
// ---------------------------------------------------------------------------

fn to_stored(p: &Path) -> String {
    if is_portable() {
        if let (Some(exe), Ok(abs)) = (exe_dir(), p.canonicalize()) {
            if let Ok(rel) = abs.strip_prefix(&exe) {
                return format!("@exe/{}", rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    p.to_string_lossy().to_string()
}

fn from_stored(s: &str) -> PathBuf {
    match s.strip_prefix("@exe/") {
        Some(rel) => exe_dir().map(|d| d.join(rel)).unwrap_or_else(|| PathBuf::from(rel)),
        None => PathBuf::from(s),
    }
}

pub fn recent_files() -> Vec<PathBuf> {
    read("recent.txt")
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(from_stored)
        .collect()
}

pub fn push_recent(p: &Path) {
    if p.as_os_str() == ":memory:" {
        return;
    }
    let mut list = recent_files();
    let canon = |x: &Path| x.canonicalize().unwrap_or_else(|_| x.to_path_buf());
    list.retain(|x| canon(x) != canon(p));
    list.insert(0, p.to_path_buf());
    list.truncate(12);
    let body: String = list.iter().map(|x| format!("{}\n", to_stored(x))).collect();
    let _ = write("recent.txt", &body);
}

// ---------------------------------------------------------------------------
// Simple key/value settings
// ---------------------------------------------------------------------------

pub fn setting(key: &str) -> Option<String> {
    read("settings.tsv")?.lines().find_map(|l| l.split_once('\t').filter(|(k, _)| *k == key).map(|(_, v)| v.to_string()))
}

pub fn set_setting(key: &str, value: &str) {
    let mut lines: Vec<(String, String)> = read("settings.tsv")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(k, v)| (k.to_string(), v.to_string())))
        .filter(|(k, _)| k != key)
        .collect();
    lines.push((key.to_string(), value.replace(['\t', '\n'], " ")));
    let body: String = lines.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
    let _ = write("settings.tsv", &body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_and_recent_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: tests touching this env var are confined to this module
        unsafe { std::env::set_var("QUARRY_DATA_DIR", dir.path()) };
        set_setting("theme", "dark");
        set_setting("theme", "light");
        assert_eq!(setting("theme").as_deref(), Some("light"));
        let db = dir.path().join("a.db");
        std::fs::write(&db, b"").unwrap();
        push_recent(&db);
        push_recent(&db);
        assert_eq!(recent_files().len(), 1);
        unsafe { std::env::remove_var("QUARRY_DATA_DIR") };
    }
}
