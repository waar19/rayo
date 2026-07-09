use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::index::FileIndex;

const INDEX_ZSTD_MAGIC: &[u8; 8] = b"RAYOZST1";

pub fn save_index(index: &FileIndex, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let temp_path = build_temp_index_path(path);
    let file = File::create(&temp_path)
        .with_context(|| format!("failed to create temp file {}", temp_path.display()))?;
    let encoded = bincode::serialize(index).with_context(|| {
        format!(
            "failed to serialize index into intermediate buffer {}",
            temp_path.display()
        )
    })?;
    let compressed = zstd::stream::encode_all(encoded.as_slice(), 1).with_context(|| {
        format!(
            "failed to compress index payload into {}",
            temp_path.display()
        )
    })?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(INDEX_ZSTD_MAGIC)
        .with_context(|| format!("failed to write header {}", temp_path.display()))?;
    writer
        .write_all(&compressed)
        .with_context(|| format!("failed to write compressed payload {}", temp_path.display()))?;
    writer
        .flush()
        .with_context(|| format!("failed to flush buffer {}", temp_path.display()))?;
    let file = writer
        .into_inner()
        .map_err(|err| err.into_error())
        .with_context(|| format!("failed to finalize write {}", temp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temp_path.display()))?;

    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
    }
    if let Err(err) = std::fs::rename(&temp_path, path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(err).with_context(|| {
            format!(
                "failed to move temp file {} to {}",
                temp_path.display(),
                path.display()
            )
        });
    }
    Ok(())
}

pub fn load_index(path: impl AsRef<Path>) -> Result<FileIndex> {
    let path = path.as_ref();
    let payload =
        std::fs::read(path).with_context(|| format!("failed to open {}", path.display()))?;
    let decoded = if payload.starts_with(INDEX_ZSTD_MAGIC) {
        zstd::stream::decode_all(&payload[INDEX_ZSTD_MAGIC.len()..])
            .with_context(|| format!("failed to decompress compressed index {}", path.display()))?
    } else {
        payload
    };
    let mut index: FileIndex = bincode::deserialize(&decoded)
        .with_context(|| format!("failed to deserialize index at {}", path.display()))?;
    index.rebuild_search_arena();
    Ok(index)
}

fn build_temp_index_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("index.rayo");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_name = format!("{file_name}.tmp-{}-{stamp}", std::process::id());
    path.with_file_name(temp_name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Write;

    use crate::{FileEntry, FileIndex};

    use super::super::index::SearchArena;

    use super::{load_index, save_index};

    #[test]
    fn round_trip_index() {
        let mut entries = HashMap::new();
        entries.insert(
            10,
            FileEntry {
                frn: 10,
                parent_frn: 5,
                name: "demo.txt".to_string(),
                attributes: 0,
            },
        );
        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 77,
            next_usn: 123,
            indexed_at_epoch_secs: 999,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();

        let path = std::env::temp_dir().join("rayo-roundtrip-test.bin");
        save_index(&index, &path).expect("save");
        let loaded = load_index(&path).expect("load");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded.drive, "C:");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.journal_id, 77);
    }

    #[test]
    fn save_index_replaces_existing_file() {
        let mut entries = HashMap::new();
        entries.insert(
            1,
            FileEntry {
                frn: 1,
                parent_frn: 1,
                name: "old.txt".to_string(),
                attributes: 0,
            },
        );
        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 10,
            next_usn: 20,
            indexed_at_epoch_secs: 30,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();

        let path = std::env::temp_dir().join("rayo-atomic-save-test.rayo");
        save_index(&index, &path).expect("save old");

        index.entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 1,
                name: "new.txt".to_string(),
                attributes: 0,
            },
        );
        index.rebuild_search_arena();
        index.journal_id = 11;
        save_index(&index, &path).expect("save new");

        let loaded = load_index(&path).expect("load new");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded.journal_id, 11);
        assert_eq!(loaded.entries.len(), 2);
        assert!(loaded.entries.contains_key(&2));
    }

    #[test]
    fn load_index_supports_legacy_uncompressed_format() {
        let mut entries = HashMap::new();
        entries.insert(
            1,
            FileEntry {
                frn: 1,
                parent_frn: 1,
                name: "legacy.txt".to_string(),
                attributes: 0,
            },
        );
        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 2,
            indexed_at_epoch_secs: 3,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();

        let legacy_bytes = bincode::serialize(&index).expect("serialize");
        let path = std::env::temp_dir().join("rayo-legacy-uncompressed.rayo");
        let mut file = std::fs::File::create(&path).expect("create file");
        file.write_all(&legacy_bytes).expect("write");
        file.flush().expect("flush");

        let loaded = load_index(&path).expect("load legacy");
        std::fs::remove_file(&path).ok();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.journal_id, 1);
    }
}
