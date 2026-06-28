// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! File I/O for Arte Ogre.
//!
//! This crate owns the native `.ogre` document format, raster import/export,
//! OpenRaster interchange, PSD import, and optional ML matte refinement.

pub mod color;
pub mod error;
pub mod interchange;
#[cfg(feature = "ml")]
pub mod matte_ml;
pub mod ogre;
pub mod raster;
#[cfg(feature = "svg")]
pub mod svg;

pub use color::{convert_to_profile, detect_embedded_profile, IccProfile};
pub use error::IoError;
pub use interchange::{export_ora, import_ora, import_psd};
pub use ogre::{load, save};
pub use raster::{export_image, import_image, ExportOptions, RasterBitDepth, RasterFormat};
#[cfg(feature = "svg")]
pub use svg::{export_svg, import_svg, import_svg_file, SvgImportMode, SvgImportOptions};
