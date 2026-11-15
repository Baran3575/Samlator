use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    Exited(i32),
    Signaled(i32),
}

impl SessionState {
    pub fn is_running(self) -> bool {
        matches!(self, SessionState::Running)
    }
}

/// A guest process (usually `proot-rs … antigravity`), with its output captured
/// into a log the UI can tail.
pub struct Session {
    label: String,
    child: Option<Child>,
    log_path: PathBuf,
}

impl Session {
    pub fn spawn(
        argv: &[String],
        env: &BTreeMap<String, String>,
        log_path: &Path,
        label: impl Into<String>,
    ) -> Result<Self> {
        let label = label.into();
        if argv.is_empty() {
            return Err(Error::Process("empty argv".into()));
        }
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        {
            let mut writer = BufWriter::new(File::create(log_path)?);
            let _ = writeln!(
                writer,
                "=== {label} @ {} ===\n$ {}",
                now_stamp(),
                argv.join(" ")
            );
            let _ = writer.flush();
        }
        let stdout = File::options().append(true).open(log_path).ok();
        let stderr = File::options().append(true).open(log_path).ok();

        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(match stdout {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            })
            .stderr(match stderr {
                Some(f) => Stdio::from(f),
                None => Stdio::null(),
            });

        // Android gives us a bare environment; the guest needs the basics back.
        for key in ["HOME", "TMPDIR", "PATH"] {
            if let Ok(v) = std::env::var(key) {
                if key == "PATH" {
                    continue;
                }
                cmd.env(key, v);
            }
        }
        for (k, v) in env {
            cmd.env(k, v);
        }

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // New process group: signals to the app must not kill the guest and
            // vice versa. PRoot gets its own session leader.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let child = cmd
            .spawn()
            .map_err(|e| Error::Process(format!("spawn {}: {e}", argv[0])))?;

        tracing::info!("started {label} pid={}", child.id());
        Ok(Self { label, child: Some(child), log_path: log_path.to_path_buf() })
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|c| c.id())
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Non-blocking poll.
    pub fn poll(&mut self) -> SessionState {
        let Some(child) = self.child.as_mut() else {
            return SessionState::Exited(0);
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(sig) = status.signal() {
                        return SessionState::Signaled(sig);
                    }
                }
                SessionState::Exited(status.code().unwrap_or(-1))
            }
            Ok(None) => SessionState::Running,
            Err(e) => {
                tracing::warn!("wait failed: {e}");
                SessionState::Exited(-1)
            }
        }
    }

    /// Politely close, then force. The whole process group goes down so PRoot's
    /// tracees do not survive as orphans.
    pub fn terminate(&mut self) {
        let Some(child) = self.child.as_mut() else { return };
        let pid = child.id() as i32;
        #[cfg(unix)]
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let _ = child.kill();
        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(_)) => break,
                _ => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
        #[cfg(unix)]
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait();
        self.child = None;
    }

    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.is_some() {
            tracing::info!("session {} dropped, terminating", self.label);
            self.terminate();
        }
    }
}

fn now_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_and_reap() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        let mut s = Session::spawn(
            &["/bin/sh".into(), "-c".into(), "echo hi; sleep 0.1".into()],
            &BTreeMap::new(),
            &log,
            "test",
        )
        .unwrap();
        assert_eq!(s.poll(), SessionState::Running);
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(s.poll(), SessionState::Exited(0));
        let text = s.log_text();
        assert!(text.contains("echo hi"), "log was: {text}");
    }

    #[test]
    fn terminate_stops_long_process() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t2.log");
        let mut s = Session::spawn(
            &["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            &BTreeMap::new(),
            &log,
            "sleeper",
        )
        .unwrap();
        s.terminate();
        assert!(!s.poll().is_running());
    }

    #[test]
    fn empty_argv_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Session::spawn(&[], &BTreeMap::new(), &dir.path().join("x"), "x").is_err());
    }
}
