//! Agent memory — persists across sessions.
//!
//! Uses a simple JSON file on disk rather than HA's `frontend/user_data`
//! (which is per-user and not designed for machine data).  The memory file
//! lives alongside the automation's other data.
//!
//! The agent's memory is free-form prose — patterns noticed, open questions,
//! accumulated evidence.  It is **not** a JSON scratchpad.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Stored memory structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryData {
    /// Schema version for future migration.
    version: u32,
    /// Free-form memory content (prose).
    content: String,
    /// ISO timestamp of last update.
    updated: String,
    /// Number of sessions that have contributed to this memory.
    session_count: u32,
}

impl Default for MemoryData {
    fn default() -> Self {
        Self {
            version: 1,
            content: String::new(),
            updated: String::new(),
            session_count: 0,
        }
    }
}

/// Agent memory backed by a JSON file.
pub struct Memory {
    path: PathBuf,
    data: MemoryData,
}

impl Memory {
    /// Load memory from disk.  If the file doesn't exist, starts empty.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        let data = if path.exists() {
            let bytes = tokio::fs::read(&path)
                .await
                .context("Failed to read memory file")?;
            match serde_json::from_slice(&bytes) {
                Ok(data) => {
                    debug!(path = %path.display(), "Loaded agent memory");
                    data
                }
                Err(e) => {
                    warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to parse memory file — starting fresh"
                    );
                    MemoryData::default()
                }
            }
        } else {
            debug!(path = %path.display(), "No memory file — starting fresh");
            MemoryData::default()
        };

        Ok(Self { path, data })
    }

    /// Create an empty memory instance without loading from disk.
    pub fn empty(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            data: MemoryData::default(),
        }
    }

    /// Get the current memory content, or None if empty.
    pub fn content(&self) -> Option<&str> {
        if self.data.content.is_empty() {
            None
        } else {
            Some(&self.data.content)
        }
    }

    /// Get the number of sessions that have contributed to this memory.
    pub fn session_count(&self) -> u32 {
        self.data.session_count
    }

    /// Save new memory content to disk.
    pub async fn save(&mut self, content: &str) -> Result<()> {
        self.data.content = content.to_string();
        self.data.updated = chrono::Utc::now().to_rfc3339();
        self.data.session_count += 1;

        // Ensure parent directory exists
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .context("Failed to create memory directory")?;
        }

        let bytes = serde_json::to_vec_pretty(&self.data)
            .context("Failed to serialize memory")?;
        tokio::fs::write(&self.path, bytes)
            .await
            .context("Failed to write memory file")?;

        debug!(
            path = %self.path.display(),
            session_count = self.data.session_count,
            "Saved agent memory"
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn load_nonexistent_file() {
        let memory = Memory::load("/tmp/signal-ha-agent-test-nonexistent.json")
            .await
            .unwrap();
        assert!(memory.content().is_none());
        assert_eq!(memory.session_count(), 0);
    }

    #[tokio::test]
    async fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.json");

        {
            let mut memory = Memory::load(&path).await.unwrap();
            assert!(memory.content().is_none());

            memory
                .save("Day 1: Garage used 3 times. Average occupancy 2m15s.")
                .await
                .unwrap();
            assert_eq!(memory.session_count(), 1);
        }

        // Reload
        {
            let memory = Memory::load(&path).await.unwrap();
            assert_eq!(
                memory.content().unwrap(),
                "Day 1: Garage used 3 times. Average occupancy 2m15s."
            );
            assert_eq!(memory.session_count(), 1);
        }
    }

    #[tokio::test]
    async fn corrupted_file_starts_fresh() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"not json").unwrap();

        let memory = Memory::load(f.path()).await.unwrap();
        assert!(memory.content().is_none());
    }
}
