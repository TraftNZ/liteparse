//! Stream a size-limited full artifact, publishing it only after a clean close.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

pub const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_PREVIEW_BYTES: u64 = 20 * 1024 * 1024;
const TEMP_CREATE_ATTEMPTS: usize = 32;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub struct LimitedWriter<W> {
    inner: W,
    remaining: u64,
}

impl<W> LimitedWriter<W> {
    pub fn new(inner: W, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(io::Error::other(
                "geometry JSON exceeds its output size limit",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct ArtifactWriter {
    destination: PathBuf,
    temporary: PathBuf,
    writer: Option<BufWriter<LimitedWriter<File>>>,
    page_count: usize,
    failed: bool,
}

impl ArtifactWriter {
    pub fn create(destination: &Path) -> io::Result<Self> {
        Self::create_with_limit(destination, MAX_ARTIFACT_BYTES)
    }

    pub fn create_with_limit(destination: &Path, limit: u64) -> io::Result<Self> {
        let name = destination.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "artifact destination has no file name",
            )
        })?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..TEMP_CREATE_ATTEMPTS {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut temporary_name = name.to_os_string();
            temporary_name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
            let temporary = parent.join(temporary_name);
            let file = match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            let mut result = Self {
                destination: destination.to_owned(),
                temporary,
                writer: Some(BufWriter::new(LimitedWriter::new(file, limit))),
                page_count: 0,
                failed: false,
            };
            result
                .writer
                .as_mut()
                .expect("open artifact")
                .write_all(b"[")?;
            return Ok(result);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot reserve artifact temporary file",
        ))
    }

    pub fn append(&mut self, page: &impl Serialize) -> Result<(), serde_json::Error> {
        if self.failed {
            return Err(serde_json::Error::io(io::Error::other(
                "artifact writer previously failed",
            )));
        }
        let writer = self.writer.as_mut().expect("open artifact");
        let result = (|| {
            if self.page_count > 0 {
                writer.write_all(b",").map_err(serde_json::Error::io)?;
            }
            serde_json::to_writer(writer, page)
        })();
        if result.is_err() {
            self.failed = true;
        } else {
            self.page_count += 1;
        }
        result
    }

    pub fn finish(mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other("cannot publish failed geometry artifact"));
        }
        let mut writer = self.writer.take().expect("open artifact");
        writer.write_all(b"]\n")?;
        writer.flush()?;
        writer.get_ref().inner.sync_all()?;
        drop(writer);
        std::fs::rename(&self.temporary, &self.destination)
    }
}

impl Drop for ArtifactWriter {
    fn drop(&mut self) {
        // Close before removing, including on platforms that prohibit unlink
        // of open files. The final destination is never removed on failure.
        drop(self.writer.take());
        let _ = std::fs::remove_file(&self.temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory() -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lit-artifact-test-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn publishes_complete_array_and_preserves_previous_file_until_finish() {
        let directory = directory();
        let destination = directory.join("geometry.json");
        std::fs::write(&destination, b"previous artifact").unwrap();
        let mut artifact = ArtifactWriter::create(&destination).unwrap();
        artifact
            .append(&serde_json::json!({"page": 1, "segments": []}))
            .unwrap();
        artifact
            .append(&serde_json::json!({"page": 2, "text": "<label>"}))
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous artifact");
        artifact.finish().unwrap();
        let pages: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
        assert_eq!(pages.as_array().unwrap().len(), 2);
        assert_eq!(pages[1]["text"], "<label>");
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn size_failure_and_abandoned_extraction_preserve_previous_artifact() {
        const LIMIT: u64 = 16;
        let directory = directory();
        let destination = directory.join("geometry.json");
        std::fs::write(&destination, b"previous artifact").unwrap();
        let mut artifact = ArtifactWriter::create_with_limit(&destination, LIMIT).unwrap();
        artifact
            .append(&"text exceeding the artifact limit")
            .unwrap();
        assert!(artifact.finish().is_err());
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous artifact");
        {
            let mut abandoned = ArtifactWriter::create(&destination).unwrap();
            abandoned.append(&serde_json::json!({"page": 1})).unwrap();
        }
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        assert_eq!(std::fs::read(&destination).unwrap(), b"previous artifact");
        assert!(ArtifactWriter::create(&directory.join("missing/geometry.json")).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn preview_size_bound_rejects_serialization() {
        const LIMIT: u64 = 8;
        let mut writer = LimitedWriter::new(Vec::new(), LIMIT);
        assert!(serde_json::to_writer(&mut writer, &vec!["a long text item"]).is_err());
        assert!(writer.inner.len() as u64 <= LIMIT);
    }
}
