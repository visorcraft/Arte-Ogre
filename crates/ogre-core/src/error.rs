// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Error types and the crate-wide [`Result`] alias.

use crate::layer::LayerId;

/// Errors that can occur when manipulating an Arte Ogre document.
#[derive(Debug, thiserror::Error)]
pub enum OgreError {
    /// The requested layer does not exist in the document.
    #[error("layer {0:?} not found")]
    LayerNotFound(LayerId),
    /// The target layer is locked and cannot be modified.
    #[error("layer {0:?} is locked")]
    LayerLocked(LayerId),
    /// The operation expected a raster layer but received a group.
    #[error("operation requires a raster layer, got a group")]
    NotRaster,
    /// The selection contains no pixels.
    #[error("selection is empty")]
    EmptySelection,
    /// A layer operation was requested that cannot be performed.
    #[error("invalid layer operation: {0}")]
    InvalidOperation(&'static str),
    /// No layer is currently active.
    #[error("no active layer")]
    NoActiveLayer,
    /// A GPU filter operation failed (e.g. device lost or readback timeout).
    #[error("filter operation failed: {0}")]
    FilterFailed(&'static str),
    /// The editor is busy with a background operation and cannot accept edits.
    #[error("busy: {0}")]
    Busy(&'static str),
}

/// Convenience alias for results in this crate.
pub type Result<T> = core::result::Result<T, OgreError>;
