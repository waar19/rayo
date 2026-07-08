use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::ntfs::{collect_changes, enumerate_mft, normalize_drive};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    // Cached lowercase name for fast case-insensitive matching in full scans.
    // Skipped from persistence to keep index files backwards-compatible.
    #[serde(skip)]
    pub name_lower: String,
    pub attributes: u32,
}

impl FileEntry {
    pub fn is_directory(&self) -> bool {
        (self.attributes & 0x10) != 0
    }

    pub fn rebuild_lowercase_cache(&mut self) {
        self.name_lower = self.name.to_ascii_lowercase();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub drive: String,
    pub entries: HashMap<u64, FileEntry>,
    pub journal_id: u64,
    pub next_usn: i64,
    pub indexed_at_epoch_secs: u64,
}

const NTFS_FILE_REFERENCE_INDEX_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const NTFS_ROOT_DIRECTORY_INDEX: u64 = 5;

impl FileIndex {
    pub fn build(drive: &str) -> Result<Self> {
        let drive = normalize_drive(drive)?;
        let snapshot = enumerate_mft(&drive)?;
        let mut index = Self {
            drive,
            entries: snapshot.entries,
            journal_id: snapshot.journal_id,
            next_usn: snapshot.next_usn,
            indexed_at_epoch_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };
        index.rebuild_lowercase_cache();
        Ok(index)
    }

    pub fn apply_journal_changes(&mut self) -> Result<usize> {
        let changes = collect_changes(&self.drive, self.journal_id, self.next_usn)?;
        for change in &changes.events {
            match change {
                JournalChange::Upsert(entry) => {
                    let mut cached = entry.clone();
                    cached.rebuild_lowercase_cache();
                    self.entries.insert(cached.frn, cached);
                }
                JournalChange::Delete(frn) => {
                    self.entries.remove(frn);
                }
            }
        }
        self.next_usn = changes.next_usn;
        self.journal_id = changes.journal_id;
        Ok(changes.events.len())
    }

    pub fn rebuild_lowercase_cache(&mut self) {
        for entry in self.entries.values_mut() {
            entry.rebuild_lowercase_cache();
        }
    }

    pub fn resolve_path(&self, frn: u64) -> Option<String> {
        if !self.entries.contains_key(&frn) {
            return None;
        }

        let mut current = frn;
        let mut segments = Vec::new();
        let mut seen = HashSet::new();

        loop {
            if is_ntfs_root_reference(current) {
                break;
            }

            let Some(entry) = self.entries.get(&current) else {
                break;
            };
            segments.push(entry.name.clone());
            if entry.parent_frn == 0 || entry.parent_frn == current || !seen.insert(current) {
                break;
            }
            current = entry.parent_frn;
        }

        segments.reverse();
        let mut path = self.drive.clone();
        let mut pushed_any_segment = false;
        for segment in segments {
            if !pushed_any_segment && (segment == "\\" || segment.is_empty()) {
                continue;
            }
            path.push('\\');
            path.push_str(&segment);
            pushed_any_segment = true;
        }
        if !pushed_any_segment {
            path.push('\\');
        }
        Some(path)
    }

    pub fn search(&self, options: &SearchOptions) -> Vec<SearchResult> {
        let normalized_query = options.query.to_ascii_lowercase();
        let limit = options.limit.max(1);
        let ext = options
            .extension
            .as_ref()
            .map(|x| x.trim_start_matches('.').to_ascii_lowercase());
        let under = options
            .under_dir
            .as_ref()
            .and_then(|raw| build_under_matcher(raw));
        let glob_matcher = options
            .glob
            .as_ref()
            .and_then(|pattern| build_glob(pattern).ok());

        let mut results: Vec<SearchResult> = self
            .entries
            .par_iter()
            .filter_map(|(_, entry)| {
                let matches_query = if entry.name_lower.is_empty() {
                    contains_ignore_ascii_case(&entry.name, &normalized_query)
                } else {
                    entry.name_lower.contains(&normalized_query)
                };
                if !matches_query {
                    return None;
                }
                if options.directories_only && !entry.is_directory() {
                    return None;
                }
                if options.files_only && entry.is_directory() {
                    return None;
                }
                if let Some(required_ext) = &ext {
                    let ext_matches = Path::new(&entry.name)
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case(required_ext))
                        .unwrap_or(false);
                    if !ext_matches {
                        return None;
                    }
                }

                let path = self.resolve_path(entry.frn)?;
                let path_lower = path.to_ascii_lowercase();
                if let Some(under_matcher) = &under {
                    if path_lower != under_matcher.exact
                        && !path_lower.starts_with(&under_matcher.prefix)
                    {
                        return None;
                    }
                }
                if let Some(matcher) = &glob_matcher {
                    let normalized_glob_path = path.replace('\\', "/");
                    if !matcher.is_match(&normalized_glob_path) {
                        return None;
                    }
                }

                Some(SearchResult {
                    frn: entry.frn,
                    path,
                    is_directory: entry.is_directory(),
                })
            })
            // Early-stop once we have enough matches to keep latency low while typing.
            .take_any(limit)
            .collect();

        // Sort only the small candidate set by perceived relevance.
        results.sort_by(|a, b| compare_relevance(a, b, &normalized_query));
        if results.len() > limit {
            results.truncate(limit);
        }
        results
    }
}

fn is_ntfs_root_reference(frn: u64) -> bool {
    (frn & NTFS_FILE_REFERENCE_INDEX_MASK) == NTFS_ROOT_DIRECTORY_INDEX
}

fn build_glob(pattern: &str) -> Result<GlobMatcher> {
    let glob = GlobBuilder::new(pattern)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("invalid glob: {pattern}"))?;
    Ok(glob.compile_matcher())
}

fn contains_ignore_ascii_case(haystack: &str, needle_lower: &str) -> bool {
    if needle_lower.is_empty() {
        return true;
    }

    let haystack = haystack.as_bytes();
    let needle = needle_lower.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }

    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn compare_relevance(a: &SearchResult, b: &SearchResult, query_lower: &str) -> std::cmp::Ordering {
    relevance_key(a, query_lower)
        .cmp(&relevance_key(b, query_lower))
        .then_with(|| a.path.cmp(&b.path))
}

fn relevance_key(result: &SearchResult, query_lower: &str) -> (u8, usize, usize) {
    let file_name = result
        .path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(result.path.as_str())
        .to_ascii_lowercase();
    let starts_with = if file_name.starts_with(query_lower) {
        0
    } else {
        1
    };
    let match_pos = file_name.find(query_lower).unwrap_or(usize::MAX);
    (starts_with, match_pos, result.path.len())
}

#[derive(Debug, Clone)]
struct UnderMatcher {
    exact: String,
    prefix: String,
}

fn build_under_matcher(raw: &str) -> Option<UnderMatcher> {
    let mut normalized = raw.trim().replace('/', "\\").to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }

    while normalized.ends_with('\\') && normalized.len() > 3 {
        normalized.pop();
    }

    if normalized.len() == 2 && normalized.ends_with(':') {
        normalized.push('\\');
    }

    let exact = normalized.clone();
    let prefix = if normalized.ends_with('\\') {
        normalized
    } else {
        format!("{normalized}\\")
    };

    Some(UnderMatcher { exact, prefix })
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub extension: Option<String>,
    pub under_dir: Option<String>,
    pub glob: Option<String>,
    pub directories_only: bool,
    pub files_only: bool,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub frn: u64,
    pub path: String,
    pub is_directory: bool,
}

#[derive(Debug, Clone)]
pub enum JournalChange {
    Upsert(FileEntry),
    Delete(u64),
}

#[derive(Debug)]
pub struct JournalBatch {
    pub events: Vec<JournalChange>,
    pub next_usn: i64,
    pub journal_id: u64,
}

#[derive(Debug)]
pub struct MftSnapshot {
    pub entries: HashMap<u64, FileEntry>,
    pub next_usn: i64,
    pub journal_id: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{FileEntry, FileIndex, SearchOptions, contains_ignore_ascii_case};

    #[test]
    fn contains_ignore_ascii_case_handles_ascii_cases() {
        assert!(contains_ignore_ascii_case("ReportClient.exe", "report"));
        assert!(contains_ignore_ascii_case("anything", ""));
        assert!(!contains_ignore_ascii_case("abc", "abcdef"));
        assert!(!contains_ignore_ascii_case("kernel32.dll", "report"));
    }

    #[test]
    fn resolve_and_filter_search() {
        let mut entries = HashMap::new();
        entries.insert(
            1,
            FileEntry {
                frn: 1,
                parent_frn: 1,
                name: "\\".to_string(),
                name_lower: "\\".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 1,
                name: "src".to_string(),
                name_lower: "src".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "main.rs".to_string(),
                name_lower: "main.rs".to_string(),
                attributes: 0,
            },
        );

        let index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
        };

        let results = index.search(&SearchOptions {
            query: "main".to_string(),
            extension: Some("rs".to_string()),
            under_dir: Some("C:\\src".to_string()),
            glob: Some("**/*.rs".to_string()),
            directories_only: false,
            files_only: true,
            limit: 10,
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "C:\\src\\main.rs");
    }

    #[test]
    fn resolve_path_without_root_record() {
        let mut entries = HashMap::new();
        entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 5,
                name: "src".to_string(),
                name_lower: "src".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "main.rs".to_string(),
                name_lower: "main.rs".to_string(),
                attributes: 0,
            },
        );

        let index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
        };

        assert_eq!(index.resolve_path(3), Some("C:\\src\\main.rs".to_string()));
    }

    #[test]
    fn under_filter_matches_directory_boundary() {
        let mut entries = HashMap::new();
        entries.insert(
            1,
            FileEntry {
                frn: 1,
                parent_frn: 1,
                name: "\\".to_string(),
                name_lower: "\\".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 1,
                name: "src".to_string(),
                name_lower: "src".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "main.rs".to_string(),
                name_lower: "main.rs".to_string(),
                attributes: 0,
            },
        );
        entries.insert(
            4,
            FileEntry {
                frn: 4,
                parent_frn: 1,
                name: "src2".to_string(),
                name_lower: "src2".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            5,
            FileEntry {
                frn: 5,
                parent_frn: 4,
                name: "main.rs".to_string(),
                name_lower: "main.rs".to_string(),
                attributes: 0,
            },
        );

        let index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
        };

        let results = index.search(&SearchOptions {
            query: "main".to_string(),
            extension: None,
            under_dir: Some("C:\\src".to_string()),
            glob: None,
            directories_only: false,
            files_only: true,
            limit: 10,
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "C:\\src\\main.rs");
    }
}
