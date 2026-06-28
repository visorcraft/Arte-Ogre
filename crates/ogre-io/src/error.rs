// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Errors returned by `ogre-io` file operations.

/// Errors that can occur while reading or writing any supported file format.
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    /// An underlying I/O operation failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Encoding/decoding through the `image` crate failed.
    #[error("image codec error: {0}")]
    Image(#[from] image::ImageError),
    /// An `ogre-core` operation failed.
    #[error("core error: {0}")]
    Core(#[from] ogre_core::OgreError),
    /// A ZIP archive operation failed.
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    /// An XML parse/serialize operation failed.
    #[error("xml error: {0}")]
    Xml(#[from] quick_xml::Error),
    /// An XML attribute parse operation failed.
    #[error("xml attribute error: {0}")]
    XmlAttr(#[from] quick_xml::events::attributes::AttrError),
    /// A PSD parse operation failed.
    #[error("psd error: {0}")]
    Psd(#[from] psd::PsdError),
    /// Encoding the manifest failed.
    #[error("serialization error: {0}")]
    Serialize(#[from] rmp_serde::encode::Error),
    /// Decoding the manifest failed.
    #[error("deserialization error: {0}")]
    Deserialize(#[from] rmp_serde::decode::Error),
    /// The file version is not supported by this build.
    #[error("unsupported file version: {0}")]
    UnsupportedVersion(u32),
    /// A tile blob did not decode to the expected pixel data.
    #[error("corrupt tile data")]
    CorruptTile,
    /// The file does not begin with the expected magic bytes.
    #[error("bad file magic")]
    BadMagic,
    /// The manifest contains inconsistent layer references.
    #[error("corrupt manifest: {0}")]
    CorruptManifest(&'static str),
    /// The requested format or feature is unsupported.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
    /// An ICC color conversion failed.
    #[error("color conversion failed: {0}")]
    ColorConversion(String),
    /// AI matte refinement (model download or ONNX inference) failed.
    #[error("AI matte error: {0}")]
    Ml(String),
    /// An SVG document could not be parsed.
    #[error("SVG parse error: {0}")]
    SvgParse(String),
    /// An SVG document could not be rendered to pixels.
    #[error("SVG render error: {0}")]
    SvgRender(String),
}
