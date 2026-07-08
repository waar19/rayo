use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

use anyhow::{Context, Result};

use crate::index::FileIndex;

pub fn save_index(index: &FileIndex, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("no se pudo crear carpeta {}", parent.display()))?;
    }
    let file =
        File::create(path).with_context(|| format!("no se pudo crear {}", path.display()))?;
    let writer = BufWriter::new(file);
    bincode::serialize_into(writer, index)
        .with_context(|| format!("no se pudo serializar indice a {}", path.display()))?;
    Ok(())
}

pub fn load_index(path: impl AsRef<Path>) -> Result<FileIndex> {
    let path = path.as_ref();
    let file = File::open(path).with_context(|| format!("no se pudo abrir {}", path.display()))?;
    let reader = BufReader::new(file);
    let index: FileIndex = bincode::deserialize_from(reader)
        .with_context(|| format!("no se pudo deserializar indice en {}", path.display()))?;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{FileEntry, FileIndex};

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
        let index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 77,
            next_usn: 123,
            indexed_at_epoch_secs: 999,
        };

        let path = std::env::temp_dir().join("rayo-roundtrip-test.bin");
        save_index(&index, &path).expect("save");
        let loaded = load_index(&path).expect("load");
        std::fs::remove_file(&path).ok();

        assert_eq!(loaded.drive, "C:");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.journal_id, 77);
    }
}
