use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use ignore::WalkBuilder;
use tree_sitter::{Language, Parser};

#[derive(Debug, Clone)]
pub struct SyntaxSearchOptions {
    pub query: String,
    pub under_dir: Option<PathBuf>,
    pub language: Option<String>,
    pub node_kind: Option<String>,
    pub limit: usize,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct SyntaxMatch {
    pub path: String,
    pub language: String,
    pub node_kind: String,
    pub line_number: u64,
    pub column_number: u64,
    pub snippet: String,
}

#[derive(Debug, Clone)]
pub struct SyntaxSearchResult {
    pub matches: Vec<SyntaxMatch>,
    pub scanned_files: usize,
    pub took: Duration,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy)]
enum SyntaxLanguage {
    Rust,
    JavaScript,
    TypeScript,
    Python,
}

impl SyntaxLanguage {
    fn from_name(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "rs" | "rust" => Some(Self::Rust),
            "js" | "javascript" => Some(Self::JavaScript),
            "ts" | "typescript" | "tsx" => Some(Self::TypeScript),
            "py" | "python" => Some(Self::Python),
            _ => None,
        }
    }

    fn from_path(path: &Path) -> Option<Self> {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.to_ascii_lowercase())?;
        match extension.as_str() {
            "rs" => Some(Self::Rust),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "tsx" => Some(Self::TypeScript),
            "py" => Some(Self::Python),
            _ => None,
        }
    }

    fn as_name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Python => "python",
        }
    }

    fn language(self) -> Language {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
        }
    }
}

pub fn search_syntax(options: &SyntaxSearchOptions) -> Result<SyntaxSearchResult> {
    let started = Instant::now();
    let query = options.query.trim().to_ascii_lowercase();
    let node_filter = options
        .node_kind
        .as_ref()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let preferred_language = options
        .language
        .as_ref()
        .and_then(|value| SyntaxLanguage::from_name(value));
    if options.language.is_some() && preferred_language.is_none() {
        return Err(anyhow!(
            "unsupported language '{}'. Supported: rust, javascript, typescript, python",
            options.language.as_deref().unwrap_or_default()
        ));
    }

    let under_dir = options
        .under_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    if !under_dir.exists() {
        return Err(anyhow!(
            "syntax search root does not exist: {}",
            under_dir.display()
        ));
    }

    let mut parser = Parser::new();
    let mut matches = Vec::new();
    let mut scanned_files = 0usize;
    let mut timed_out = false;
    let limit = options.limit.max(1);
    let timeout = options.timeout.max(Duration::from_millis(100));

    let mut walker = WalkBuilder::new(&under_dir);
    walker
        .standard_filters(true)
        .hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    for entry in walker.build() {
        if started.elapsed() >= timeout {
            timed_out = true;
            break;
        }
        if matches.len() >= limit {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }

        let Some(language) = preferred_language.or_else(|| SyntaxLanguage::from_path(path)) else {
            continue;
        };

        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(_) => continue,
        };
        scanned_files += 1;
        parser
            .set_language(&language.language())
            .with_context(|| format!("failed to set parser language for {}", language.as_name()))?;
        let Some(tree) = parser.parse(source.as_str(), None) else {
            continue;
        };

        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if started.elapsed() >= timeout {
                timed_out = true;
                break;
            }
            if matches.len() >= limit {
                break;
            }

            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() {
                    stack.push(child);
                }
            }

            if !node.is_named() {
                continue;
            }
            if let Some(filter) = node_filter.as_deref()
                && node.kind().to_ascii_lowercase() != filter
            {
                continue;
            }

            let Ok(text) = node.utf8_text(source.as_bytes()) else {
                continue;
            };
            if !query.is_empty() && !text.to_ascii_lowercase().contains(&query) {
                continue;
            }

            let start = node.start_position();
            let snippet = text.lines().next().unwrap_or("").trim().to_string();
            matches.push(SyntaxMatch {
                path: path.display().to_string(),
                language: language.as_name().to_string(),
                node_kind: node.kind().to_string(),
                line_number: (start.row + 1) as u64,
                column_number: (start.column + 1) as u64,
                snippet: truncate_snippet(snippet),
            });
        }
        if timed_out || matches.len() >= limit {
            break;
        }
    }

    Ok(SyntaxSearchResult {
        matches,
        scanned_files,
        took: started.elapsed(),
        timed_out,
    })
}

fn truncate_snippet(mut snippet: String) -> String {
    const MAX_LEN: usize = 180;
    if snippet.len() <= MAX_LEN {
        return snippet;
    }
    snippet.truncate(MAX_LEN);
    snippet.push_str("...");
    snippet
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use super::{SyntaxSearchOptions, search_syntax};

    #[test]
    fn syntax_search_finds_rust_function_node() {
        let temp_dir = std::env::temp_dir().join(format!("rayo-syntax-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let file_path = temp_dir.join("sample.rs");
        fs::write(
            &file_path,
            r#"
fn hello_world() {
    println!("hello");
}
"#,
        )
        .expect("write test file");

        let result = search_syntax(&SyntaxSearchOptions {
            query: "hello_world".to_string(),
            under_dir: Some(temp_dir.clone()),
            language: Some("rust".to_string()),
            node_kind: Some("function_item".to_string()),
            limit: 10,
            timeout: Duration::from_secs(2),
        })
        .expect("search");

        fs::remove_file(&file_path).ok();
        fs::remove_dir(&temp_dir).ok();

        assert!(!result.matches.is_empty());
        assert!(
            result
                .matches
                .iter()
                .any(|item| item.node_kind == "function_item")
        );
    }
}
