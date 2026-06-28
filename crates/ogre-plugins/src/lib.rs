// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Plugin host for Arte Ogre.
//!
//! Defines the stable WIT contract and a `wasmtime`-based host harness for
//! running untrusted tile-filter plugins.

pub mod filter;
pub mod manager;
pub mod script;

pub use filter::PluginFilter;
pub use manager::{PluginInfo, PluginKind, PluginManager};
pub use script::{ScriptEngine, ScriptError};

use thiserror::Error;
use wasmtime::{Memory, Store, StoreLimits};

/// Hard cap on a plugin instance's linear memory.
///
/// Fuel bounds *instructions* but not *bytes*; without a memory limit a hostile
/// plugin can `memory.grow` until the host runs out of RAM. 512 MiB is ample for
/// even a 4K RGBA32F tile (input + output) while bounding a runaway allocation.
pub(crate) const MAX_PLUGIN_MEMORY: usize = 512 * 1024 * 1024;

/// Errors returned by the plugin host.
#[derive(Debug, Error)]
pub enum PluginError {
    /// An error from the underlying WASM runtime.
    #[error("wasmtime error: {0}")]
    Wasmtime(#[from] wasmtime::Error),
    /// An error accessing plugin linear memory.
    #[error("memory access error: {0}")]
    MemoryAccess(#[from] wasmtime::MemoryAccessError),
    /// The plugin module is missing a required export.
    #[error("plugin missing required export `{0}`")]
    MissingExport(&'static str),
    /// The plugin returned an output tile of an unexpected size.
    #[error("plugin returned invalid output length")]
    InvalidOutputLength,
}

/// Host harness for running tile-filter plugins.
///
/// The production path is [`PluginFilter`], which runs a WASM tile-filter plugin
/// tile-by-tile on the CPU via the shared [`grow_memory_to`] helper and the
/// process-wide engine/module cache.
pub(crate) fn grow_memory_to(
    store: &mut Store<StoreLimits>,
    memory: &Memory,
    size: usize,
) -> Result<(), PluginError> {
    const PAGE: usize = 64 * 1024;
    let current = memory.data(&mut *store).len();
    if size <= current {
        return Ok(());
    }
    let delta = (size - current).div_ceil(PAGE) as u64;
    memory.grow(store, delta)?;
    Ok(())
}
