use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::AtomicUsize;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use memchr::memmem::Finder;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use serde::{Deserialize, Serialize};

use crate::ntfs::{collect_changes, enumerate_mft_with_progress, normalize_drive};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub frn: u64,
    pub parent_frn: u64,
    pub name: String,
    pub attributes: u32,
}

impl FileEntry {
    pub fn is_directory(&self) -> bool {
        (self.attributes & 0x10) != 0
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SearchArena {
    names: Vec<u8>,
    offsets: Vec<u32>,
    frns: Vec<u64>,
    dirs: Vec<bool>,
    tombstones: Vec<bool>,
    tombstone_count: usize,
    slot_by_frn: HashMap<u64, usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct TrigramIndex {
    postings: HashMap<u32, Vec<u64>>,
}

impl TrigramIndex {
    fn from_entries<'a>(entries: impl Iterator<Item = &'a FileEntry>) -> Self {
        let mut postings: HashMap<u32, Vec<u64>> = HashMap::new();
        for entry in entries {
            let lowered = entry.name.to_ascii_lowercase();
            let grams = trigrams_from_lower(&lowered);
            if grams.is_empty() {
                continue;
            }
            for gram in grams {
                postings.entry(gram).or_default().push(entry.frn);
            }
        }

        for ids in postings.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }
        Self { postings }
    }

    fn query_candidates(&self, query_lower: &str, max_candidates: usize) -> Vec<u64> {
        if max_candidates == 0 {
            return Vec::new();
        }

        let grams = trigrams_from_lower(query_lower);
        if grams.is_empty() {
            return Vec::new();
        }

        let mut posting_lists: Vec<&Vec<u64>> = grams
            .iter()
            .filter_map(|gram| self.postings.get(gram))
            .collect();
        if posting_lists.len() != grams.len() {
            return Vec::new();
        }

        posting_lists.sort_by_key(|list| list.len());
        let seed = posting_lists[0];
        let rest = &posting_lists[1..];

        let mut matches = Vec::with_capacity(seed.len().min(max_candidates));
        'seed_loop: for frn in seed {
            for list in rest {
                if list.binary_search(frn).is_err() {
                    continue 'seed_loop;
                }
            }
            matches.push(*frn);
            if matches.len() >= max_candidates {
                break;
            }
        }
        matches
    }
}

impl SearchArena {
    fn from_entries<'a>(entries: impl Iterator<Item = &'a FileEntry>) -> Self {
        let mut arena = Self::default();
        for entry in entries {
            arena.append_entry(entry);
        }
        arena
    }

    fn append_entry(&mut self, entry: &FileEntry) {
        let start = self.names.len();
        if start > u32::MAX as usize {
            return;
        }

        self.offsets.push(start as u32);
        self.names
            .extend(entry.name.bytes().map(|byte| byte.to_ascii_lowercase()));
        self.names.push(0);
        self.frns.push(entry.frn);
        self.dirs.push(entry.is_directory());
        self.tombstones.push(false);

        let slot = self.frns.len() - 1;
        self.slot_by_frn.insert(entry.frn, slot);
    }

    fn delete_frn(&mut self, frn: u64) {
        let Some(slot) = self.slot_by_frn.remove(&frn) else {
            return;
        };

        if !self.tombstones.get(slot).copied().unwrap_or(true) {
            self.tombstones[slot] = true;
            self.tombstone_count += 1;
        }
    }

    fn upsert_entry(&mut self, entry: &FileEntry) {
        self.delete_frn(entry.frn);
        self.append_entry(entry);
    }

    fn should_compact(&self) -> bool {
        self.frns.len() >= 1024 && self.tombstone_count.saturating_mul(10) >= self.frns.len()
    }

    fn live_slots(&self) -> usize {
        self.frns.len().saturating_sub(self.tombstone_count)
    }

    fn candidate_frns(&self, query_lower: &str, max_candidates: usize) -> Vec<u64> {
        if max_candidates == 0 || self.frns.is_empty() {
            return Vec::new();
        }

        if query_lower.is_empty() {
            let mut results = Vec::with_capacity(max_candidates.min(self.live_slots()));
            for (slot, frn) in self.frns.iter().enumerate() {
                if self.tombstones.get(slot).copied().unwrap_or(true) {
                    continue;
                }
                results.push(*frn);
                if results.len() >= max_candidates {
                    break;
                }
            }
            return results;
        }

        let query = query_lower.as_bytes();
        if query.contains(&0) {
            return Vec::new();
        }

        let chunk_target = rayon::current_num_threads().max(1) * 2;
        let slots_per_chunk = self.frns.len().div_ceil(chunk_target).max(1);
        let mut chunks = Vec::new();
        let mut start = 0usize;
        while start < self.frns.len() {
            let end = (start + slots_per_chunk).min(self.frns.len());
            chunks.push((start, end));
            start = end;
        }

        let chunk_results: Vec<Vec<u64>> = chunks
            .into_par_iter()
            .map(|(start_slot, end_slot)| {
                let finder = Finder::new(query);
                let start_byte = self.offsets[start_slot] as usize;
                let end_byte = if end_slot < self.offsets.len() {
                    self.offsets[end_slot] as usize
                } else {
                    self.names.len()
                };

                let slice = &self.names[start_byte..end_byte];
                let mut matches = Vec::new();
                let mut slot = start_slot;
                let mut next_slot_start = if slot + 1 < self.offsets.len() {
                    self.offsets[slot + 1] as usize
                } else {
                    self.names.len()
                };
                let mut last_emitted = None;

                for relative in finder.find_iter(slice) {
                    let global = start_byte + relative;
                    while slot + 1 < end_slot && global >= next_slot_start {
                        slot += 1;
                        next_slot_start = if slot + 1 < self.offsets.len() {
                            self.offsets[slot + 1] as usize
                        } else {
                            self.names.len()
                        };
                    }

                    if slot >= end_slot {
                        break;
                    }
                    if self.tombstones.get(slot).copied().unwrap_or(true) {
                        continue;
                    }
                    if last_emitted == Some(slot) {
                        continue;
                    }

                    matches.push(self.frns[slot]);
                    last_emitted = Some(slot);
                    if matches.len() >= max_candidates {
                        break;
                    }
                }

                matches
            })
            .collect();

        let mut merged = Vec::with_capacity(max_candidates.min(self.live_slots()));
        for mut chunk in chunk_results {
            if merged.len() >= max_candidates {
                break;
            }
            let left = max_candidates - merged.len();
            if chunk.len() > left {
                chunk.truncate(left);
            }
            merged.extend(chunk);
        }
        merged
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileIndex {
    pub drive: String,
    pub entries: HashMap<u64, FileEntry>,
    pub journal_id: u64,
    pub next_usn: i64,
    pub indexed_at_epoch_secs: u64,
    #[serde(skip, default)]
    pub(crate) search_arena: SearchArena,
    #[serde(skip, default)]
    pub(crate) trigram_index: Option<TrigramIndex>,
}

const NTFS_FILE_REFERENCE_INDEX_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const NTFS_ROOT_DIRECTORY_INDEX: u64 = 5;
const TRIGRAM_MIN_QUERY_LEN: usize = 8;

impl FileIndex {
    pub fn build(drive: &str) -> Result<Self> {
        Self::build_with_progress(drive, None)
    }

    pub fn build_with_progress(
        drive: &str,
        progress_counter: Option<&AtomicUsize>,
    ) -> Result<Self> {
        let drive = normalize_drive(drive)?;
        let snapshot = enumerate_mft_with_progress(&drive, progress_counter)?;
        let mut index = Self {
            drive,
            entries: snapshot.entries,
            journal_id: snapshot.journal_id,
            next_usn: snapshot.next_usn,
            indexed_at_epoch_secs: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();
        Ok(index)
    }

    pub fn apply_journal_changes(&mut self) -> Result<usize> {
        let changes = collect_changes(&self.drive, self.journal_id, self.next_usn)?;
        for change in &changes.events {
            match change {
                JournalChange::Upsert(entry) => {
                    let cached = entry.clone();
                    self.entries.insert(cached.frn, cached);
                    if let Some(updated) = self.entries.get(&entry.frn) {
                        self.search_arena.upsert_entry(updated);
                    }
                }
                JournalChange::Delete(frn) => {
                    self.entries.remove(frn);
                    self.search_arena.delete_frn(*frn);
                }
            }
        }
        self.next_usn = changes.next_usn;
        self.journal_id = changes.journal_id;
        if self.search_arena.should_compact() {
            self.rebuild_search_arena();
        }
        if !changes.events.is_empty() && self.trigram_index.is_some() {
            self.rebuild_trigram_index();
        }
        Ok(changes.events.len())
    }

    pub fn rebuild_search_arena(&mut self) {
        self.search_arena = SearchArena::from_entries(self.entries.values());
    }

    pub fn rebuild_trigram_index(&mut self) {
        self.trigram_index = Some(TrigramIndex::from_entries(self.entries.values()));
    }

    pub fn set_trigram_enabled(&mut self, enabled: bool) {
        if enabled {
            self.rebuild_trigram_index();
        } else {
            self.trigram_index = None;
        }
    }

    pub fn trigram_enabled(&self) -> bool {
        self.trigram_index.is_some()
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
        let candidate_limit = limit.saturating_mul(8).max(limit);
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

        let trigram_candidate_limit = candidate_limit.saturating_mul(32).max(candidate_limit);
        let candidate_frns =
            if options.prefer_trigram && normalized_query.len() >= TRIGRAM_MIN_QUERY_LEN {
                if let Some(trigram) = &self.trigram_index {
                    trigram.query_candidates(&normalized_query, trigram_candidate_limit)
                } else {
                    self.search_arena
                        .candidate_frns(&normalized_query, candidate_limit)
                }
            } else {
                self.search_arena
                    .candidate_frns(&normalized_query, candidate_limit)
            };

        let mut results = Vec::new();
        for frn in candidate_frns {
            let Some(entry) = self.entries.get(&frn) else {
                continue;
            };
            if options.directories_only && !entry.is_directory() {
                continue;
            }
            if options.files_only && entry.is_directory() {
                continue;
            }
            if let Some(required_ext) = &ext {
                let ext_matches = Path::new(&entry.name)
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case(required_ext))
                    .unwrap_or(false);
                if !ext_matches {
                    continue;
                }
            }

            let Some(path) = self.resolve_path(entry.frn) else {
                continue;
            };
            let path_lower = path.to_ascii_lowercase();
            if let Some(under_matcher) = &under {
                if path_lower != under_matcher.exact
                    && !path_lower.starts_with(&under_matcher.prefix)
                {
                    continue;
                }
            }
            if let Some(matcher) = &glob_matcher {
                let normalized_glob_path = path.replace('\\', "/");
                if !matcher.is_match(&normalized_glob_path) {
                    continue;
                }
            }

            results.push(SearchResult {
                frn: entry.frn,
                path,
                is_directory: entry.is_directory(),
            });
        }

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

fn trigrams_from_lower(value: &str) -> Vec<u32> {
    let bytes = value.as_bytes();
    if bytes.len() < 3 {
        return Vec::new();
    }

    let mut seen = HashSet::new();
    let mut grams = Vec::new();
    for window in bytes.windows(3) {
        if window.contains(&0) {
            continue;
        }
        let gram = ((window[0] as u32) << 16) | ((window[1] as u32) << 8) | (window[2] as u32);
        if seen.insert(gram) {
            grams.push(gram);
        }
    }
    grams
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
    pub prefer_trigram: bool,
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

    use super::{FileEntry, FileIndex, SearchArena, SearchOptions};

    #[test]
    fn search_arena_tombstones_and_upserts() {
        let original = FileEntry {
            frn: 10,
            parent_frn: 5,
            name: "report.txt".to_string(),
            attributes: 0,
        };
        let replacement = FileEntry {
            frn: 10,
            parent_frn: 5,
            name: "report-final.txt".to_string(),
            attributes: 0,
        };

        let mut arena = SearchArena::default();
        arena.upsert_entry(&original);
        assert_eq!(arena.candidate_frns("report", 10), vec![10]);

        arena.delete_frn(10);
        assert!(arena.candidate_frns("report", 10).is_empty());

        arena.upsert_entry(&replacement);
        assert_eq!(arena.candidate_frns("final", 10), vec![10]);
    }

    #[test]
    fn search_arena_does_not_match_across_name_boundary() {
        let mut arena = SearchArena::default();
        arena.upsert_entry(&FileEntry {
            frn: 1,
            parent_frn: 1,
            name: "abc".to_string(),
            attributes: 0,
        });
        arena.upsert_entry(&FileEntry {
            frn: 2,
            parent_frn: 1,
            name: "def".to_string(),
            attributes: 0,
        });

        assert!(arena.candidate_frns("cd", 10).is_empty());
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
                attributes: 0x10,
            },
        );
        entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 1,
                name: "src".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "main.rs".to_string(),
                attributes: 0,
            },
        );

        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();

        let results = index.search(&SearchOptions {
            query: "main".to_string(),
            extension: Some("rs".to_string()),
            under_dir: Some("C:\\src".to_string()),
            glob: Some("**/*.rs".to_string()),
            directories_only: false,
            files_only: true,
            limit: 10,
            prefer_trigram: false,
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
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "main.rs".to_string(),
                attributes: 0,
            },
        );

        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();

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
                attributes: 0x10,
            },
        );
        entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 1,
                name: "src".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "main.rs".to_string(),
                attributes: 0,
            },
        );
        entries.insert(
            4,
            FileEntry {
                frn: 4,
                parent_frn: 1,
                name: "src2".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            5,
            FileEntry {
                frn: 5,
                parent_frn: 4,
                name: "main.rs".to_string(),
                attributes: 0,
            },
        );

        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();

        let results = index.search(&SearchOptions {
            query: "main".to_string(),
            extension: None,
            under_dir: Some("C:\\src".to_string()),
            glob: None,
            directories_only: false,
            files_only: true,
            limit: 10,
            prefer_trigram: false,
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "C:\\src\\main.rs");
    }

    #[test]
    fn trigram_mode_finds_expected_match() {
        let mut entries = HashMap::new();
        entries.insert(
            1,
            FileEntry {
                frn: 1,
                parent_frn: 1,
                name: "\\".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            2,
            FileEntry {
                frn: 2,
                parent_frn: 1,
                name: "Reports".to_string(),
                attributes: 0x10,
            },
        );
        entries.insert(
            3,
            FileEntry {
                frn: 3,
                parent_frn: 2,
                name: "ticket_report_2026.pdf".to_string(),
                attributes: 0,
            },
        );

        let mut index = FileIndex {
            drive: "C:".to_string(),
            entries,
            journal_id: 1,
            next_usn: 0,
            indexed_at_epoch_secs: 0,
            search_arena: SearchArena::default(),
            trigram_index: None,
        };
        index.rebuild_search_arena();
        index.set_trigram_enabled(true);

        let results = index.search(&SearchOptions {
            query: "report".to_string(),
            extension: Some("pdf".to_string()),
            under_dir: Some("C:\\Reports".to_string()),
            glob: None,
            directories_only: false,
            files_only: true,
            limit: 10,
            prefer_trigram: true,
        });

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, "C:\\Reports\\ticket_report_2026.pdf");
    }
}
