//! GPUI view layer for Roam.
//!
//! Views hold a [`roam_core::Vfs`], never an `Operator`, and reach the backend
//! only through it — see `roam_core::rt` for why that boundary matters.

pub mod actions;
mod assets;
mod browser;
mod connection_form;
mod delegate;
mod dir_tree;
mod name_dialog;
pub mod placeholders;
mod preview;
mod transfer_panel;
mod workspace;

pub use actions::init;
pub use assets::Assets;
pub use browser::Browser;
pub use connection_form::{ConnectionForm, Draft};
pub use delegate::{EntriesDelegate, kind_label};
pub use dir_tree::DirTreeView;
pub use name_dialog::NameDialog;
pub use preview::PreviewPanel;
pub use transfer_panel::TransferPanel;
pub use workspace::Workspace;
