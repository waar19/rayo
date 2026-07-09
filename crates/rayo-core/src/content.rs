use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::SearcherBuilder;
use grep_searcher::sinks::UTF8;
use ignore::WalkBuilder;

#[derive(Debug, Clone)]
pub struct ContentSearchOptions {
    pub query: String,
    pub under_dir: Option<PathBuf>,
    pub extension: Option<String>,
    pub limit: usize,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ContentMatch {
    pub path: String,
    pub line_number: u64,
    pub line_text: String,
}

#[derive(Debug, Clone)]
pub struct ContentSearchResult {
    pub matches: Vec<ContentMatch>,
    pub scanned_files: usize,
    pub took: Duration,
    pub timed_out: bool,
}

pub fn search_content(options: &ContentSearchOptions) -> Result<ContentSearchResult> {
    let scope = options
        .under_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(r"C:\"));
    if !scope.exists() {
        return Err(anyhow!("under path does not exist: {}", scope.display()));
    }

    let extension = options
        .extension
        .as_deref()
        .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let limit = options.limit.max(1);
    let timeout = if options.timeout.is_zero() {
        Duration::from_secs(3)
    } else {
        options.timeout
    };

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(true)
        .build(&options.query)
        .map_err(|err| anyhow!("invalid regex query: {err}"))?;
    let mut searcher = SearcherBuilder::new().line_number(true).build();

    let started = Instant::now();
    let mut scanned_files = 0usize;
    let mut timed_out = false;
    let mut outputs = Vec::new();
    let walker = WalkBuilder::new(&scope).standard_filters(true).build();
    for entry in walker {
        if started.elapsed() >= timeout {
            timed_out = true;
            break;
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !extension_matches(path, extension.as_deref()) {
            continue;
        }
        scanned_files += 1;
        if outputs.len() >= limit {
            break;
        }

        let display_path = path.display().to_string();
        if let Err(err) = searcher.search_path(
            &matcher,
            path,
            UTF8(|line_number, line| {
                if outputs.len() >= limit || started.elapsed() >= timeout {
                    timed_out = started.elapsed() >= timeout;
                    return Ok(false);
                }
                outputs.push(ContentMatch {
                    path: display_path.clone(),
                    line_number,
                    line_text: line.trim_end().to_string(),
                });
                Ok(true)
            }),
        ) {
            let err_text = err.to_string();
            if err_text.contains("invalid utf-8 sequence") {
                continue;
            }
            return Err(anyhow!(
                "content search failed on {}: {err}",
                path.display()
            ));
        }
    }

    Ok(ContentSearchResult {
        matches: outputs,
        scanned_files,
        took: started.elapsed(),
        timed_out,
    })
}

fn extension_matches(path: &Path, expected_ext: Option<&str>) -> bool {
    let Some(expected_ext) = expected_ext else {
        return true;
    };
    let file_ext = path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    file_ext.as_deref() == Some(expected_ext)
}
