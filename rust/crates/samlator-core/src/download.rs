use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::{Error, Progress, ProgressFn, Result};

/// Compute the lowercase hex sha256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Download `url` into `dest`, verifying the checksum when one is supplied.
///
/// Writes to `<dest>.part` first so an interrupted transfer never looks like a
/// valid, installed asset on the next launch.
pub fn fetch(
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    stage: &str,
    progress: ProgressFn<'_>,
) -> Result<()> {
    if dest.is_file() && !expected_sha256.is_empty() {
        if sha256_file(dest)? == expected_sha256 {
            progress(Progress::Stage(format!("{stage} (cached)")));
            progress(Progress::Done);
            return Ok(());
        }
        let _ = std::fs::remove_file(dest);
    }

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let part = dest.with_extension("part");

    progress(Progress::Stage(stage.to_string()));

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .timeout_read(std::time::Duration::from_secs(60))
        .user_agent(concat!("Samlator/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = agent
        .get(url)
        .call()
        .map_err(|e| Error::Http(format!("{stage}: {e}")))?;

    let total: Option<u64> = resp
        .header("Content-Length")
        .and_then(|v| v.parse().ok());

    let mut reader = resp.into_reader();
    let mut out = File::create(&part)?;
    let mut hasher = Sha256::new();
    let mut done: u64 = 0;
    let mut buf = vec![0u8; 256 * 1024];
    let mut last_report = 0u64;

    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
        done += n as u64;
        if done - last_report > 4 * 1024 * 1024 {
            last_report = done;
            progress(Progress::Bytes { done, total });
        }
    }
    out.flush()?;
    drop(out);

    if !expected_sha256.is_empty() {
        let actual = hex(&hasher.finalize());
        if actual != expected_sha256 {
            let _ = std::fs::remove_file(&part);
            return Err(Error::Checksum {
                what: stage.to_string(),
                expected: expected_sha256.to_string(),
                actual,
            });
        }
    } else {
        tracing::warn!("{stage}: no checksum published, accepting download");
    }

    std::fs::rename(&part, dest)?;
    progress(Progress::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_known_vector() {
        // sha256("abc")
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn file_hash_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), sha256_bytes(b"abc"));
    }
}
