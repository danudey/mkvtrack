//! mkvtrack - a self contained Matroska track inspector and editor.
//!
//! * [`ebml`] holds the EBML primitives and a lossless element tree.
//! * [`mkv`] locates the Segment, its children and the Tracks element.
//! * [`edit`] writes an edited Tracks element back, in place where it fits.
//! * [`scan`] reads a whole directory on background threads.
//! * [`app`] is the interface state and the edits it can apply.
//! * [`ui`] renders it.

pub mod app;
pub mod ebml;
pub mod edit;
pub mod mkv;
pub mod scan;
pub mod ui;
