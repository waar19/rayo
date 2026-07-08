mod index;
mod ntfs;
mod persist;

pub use index::{FileEntry, FileIndex, SearchOptions, SearchResult};
pub use ntfs::{is_running_as_admin, normalize_drive};
pub use persist::{load_index, save_index};
