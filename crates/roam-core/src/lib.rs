//! Backend-agnostic core for Roam.
//!
//! Nothing in this crate depends on `gpui`, so all of it is testable without a
//! window. Everything that talks to a storage backend goes through [`Vfs`], and
//! everything that touches the tokio runtime goes through [`Rt`].

pub mod cache;
pub mod error;
pub mod fmt;
pub mod menu;
pub mod model;
pub mod path;
pub mod preview;
pub mod profile;
pub mod rt;
pub mod secrets;
pub mod transfer;
pub mod tree;
pub mod vfs;

pub use cache::{CacheEntry, ListingCache};
pub use error::{Error, Recovery, Result};
pub use menu::{EntryAction, MenuItem};
pub use model::{
    DirEntry, EntryKind, Generation, ObjectVersion, RemotePath, SessionId, SortKey, matches_filter,
    sort_indices, view_indices,
};
pub use preview::{ImageKind, PreviewKind};
pub use profile::{Profile, ProfileId, ProfileStore};
pub use rt::Rt;
pub use secrets::{Keychain, MemorySecrets, SecretStore};
pub use transfer::{
    Operation, TaskId, TaskProgress, TaskSnapshot, TaskState, Transfer, TransferEngine,
    plan_download, plan_duplicate_dir, plan_move_dir, plan_upload,
};
pub use tree::{DirTree, TreeRow};
pub use vfs::{Listing, Vfs};
