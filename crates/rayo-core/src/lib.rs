mod content;
mod index;
mod ntfs;
mod persist;
mod syntax;

pub use content::{ContentMatch, ContentSearchOptions, ContentSearchResult, search_content};
pub use index::{FileEntry, FileIndex, SearchOptions, SearchResult};
pub use ntfs::{is_running_as_admin, normalize_drive};
pub use persist::{load_index, save_index};
pub use syntax::{SyntaxMatch, SyntaxSearchOptions, SyntaxSearchResult, search_syntax};
