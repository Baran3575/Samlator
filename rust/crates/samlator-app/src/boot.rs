//! Crash and boot diagnostics.
//!
//! An Android app that dies before its first frame is invisible: there is no
//! console to read. So every boot step is appended to `sys/logs/boot.log`, and a
//! panic hook stores the message both there and in memory so the UI can show it
//! on the next launch.

use std::panic::PanicHookInfo;
use std::sync::Mutex;

use samlator_core::paths::Paths;

static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Append one line to `sys/logs/boot.log` and to the shared boot log below,
/// ignoring I/O errors.
pub fn note(paths: &Paths, msg: impl AsRef<str>) {
    let msg = msg.as_ref();
    log::info!("{msg}");
    let dir = paths.logs();
    let _ = std::fs::create_dir_all(&dir);
    fan_out(dir.join("boot.log"), &format!("{msg}\n"));
}

/// The app's private dirs are unreadable on a stock phone, so the same lines go
/// to `/sdcard/Android/media/dev.samlator/boot.log`, which `Termux` can `cat`.
pub fn shared_log() -> std::path::PathBuf {
    shared_logs()
        .into_iter()
        .next()
        .unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp/samlator-boot.log"))
}

/// Every place worth mirroring the boot log to, best first.
pub fn shared_logs() -> Vec<std::path::PathBuf> {
    let mut out = vec![
        std::path::PathBuf::from("/sdcard/Android/media/dev.samlator/boot.log"),
        std::path::PathBuf::from("/storage/emulated/0/Android/media/dev.samlator/boot.log"),
    ];
    if let Some(ext) = std::env::var_os("SAMLATOR_EXT") {
        out.push(std::path::PathBuf::from(ext).join("boot.log"));
    }
    out
}

/// Write to the private log and to every shared location.
fn fan_out(private: std::path::PathBuf, line: &str) {
    append(private, line);
    for p in shared_logs() {
        append(p, line);
    }
}

fn append(path: std::path::PathBuf, line: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
        let _ = f.flush();
    }
}

/// Append one line using an already-known data dir (startup path only).
pub fn note_path_buf(base: &std::path::Path, msg: &str) {
    log::info!("{msg}");
    fan_out(base.join("sys").join("logs").join("boot.log"), &format!("{msg}\n"));
}

/// Turn a `catch_unwind` payload into a readable sentence.
pub fn describe_panic(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "bilinmeyen panic".to_string()
    }
}

/// Take the panic message recorded during this (or a previous) run.
pub fn take_last_panic() -> Option<String> {
    let mem = LAST_PANIC.lock().ok().and_then(|mut g| g.take());
    if mem.is_some() {
        return mem;
    }
    None
}

/// Route panics to logcat and to `sys/logs/boot.log` instead of vanishing.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        let text = format!("PANIC {}", describe(info));
        if let Ok(mut slot) = LAST_PANIC.lock() {
            *slot = Some(text.clone());
        }
        log::error!("{text}");
        for p in shared_logs() {
            append(p, &format!("{text}\n"));
        }
        // No `Paths` here: the hook may run before the app knows its data dir.
        if let Some(dir) = log_dir() {
            append(dir.join("boot.log"), &format!("{text}\n"));
        }
        previous(info);
    }));
}

fn describe(info: &PanicHookInfo<'_>) -> String {
    let payload = info.payload();
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    };
    match info.location() {
        Some(loc) => format!("{msg} ({}:{})", loc.file(), loc.line()),
        None => msg,
    }
}

fn log_dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("SAMLATOR_HOME")?;
    let dir = std::path::PathBuf::from(base).join("sys").join("logs");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use samlator_core::config::Config;

    #[test]
    fn notes_land_in_the_boot_log() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::resolve(Some(dir.path()));
        note(&paths, "birinci");
        note(&paths, "ikinci");
        let text = std::fs::read_to_string(paths.logs().join("boot.log")).unwrap();
        assert!(text.contains("birinci"));
        assert!(text.contains("ikinci"));
    }

    #[test]
    fn panic_payloads_become_messages() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("patladı");
        assert_eq!(describe_panic(&payload), "patladı");
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("yine"));
        assert_eq!(describe_panic(&payload), "yine");
        let _ = Config::default();
    }
}
