//! Filesystem blob storage.

mod blob;

pub use blob::FsBlobStore;
pub use blob::{rename_durable, sync_dir, write_file_atomic};
