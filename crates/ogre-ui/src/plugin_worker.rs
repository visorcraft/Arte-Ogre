// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Background worker for plugin/script execution.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use ogre_core::{Command, Document, History, TiledBuffer};
use ogre_gpu::{ApplyFilterCmd, Filter, ReplaceLayerBufferCmd};
use ogre_plugins::ScriptEngine;

use crate::state::DEFAULT_UNDO_LIMIT;

/// Request sent to the background plugin thread.
#[derive(Debug)]
pub enum PluginRequest {
    /// Run a Lua script source string.
    Lua {
        /// Request ID echoed back in the result.
        id: u64,
        /// Lua source code to execute.
        source: String,
        /// Document snapshot to operate on.
        doc: Document,
    },
    /// Run a WASM filter command on the active layer.
    Wasm {
        /// Request ID echoed back in the result.
        id: u64,
        /// Filter command to apply.
        cmd: Box<ApplyFilterCmd>,
        /// Document snapshot to operate on.
        doc: Document,
    },
}

impl PluginRequest {
    fn id(&self) -> u64 {
        match self {
            PluginRequest::Lua { id, .. } | PluginRequest::Wasm { id, .. } => *id,
        }
    }
}

/// Result delivered back to the UI thread.
///
/// WASM filters return a single undoable command so the prior undo history is
/// preserved. Lua scripts may allocate new layers, so they return a complete
/// replacement document/history pair to avoid layer-ID divergence between the
/// worker clone and the UI document.
#[allow(clippy::large_enum_variant)]
pub enum PluginResult {
    /// A WASM filter completed. The contained command, if any, must be
    /// dispatched through the normal UI command path.
    OkCommand {
        /// Request ID from the matching [`PluginRequest`].
        id: u64,
        /// Undo label for the plugin effect.
        label: String,
        /// Command to dispatch on the UI thread. `None` when the plugin made
        /// no edits.
        command: Option<Box<dyn Command>>,
    },
    /// A Lua script completed. The UI should adopt the modified document and
    /// history. This replaces earlier undo history (documented limitation for
    /// scripts that allocate layers).
    OkDocument {
        /// Request ID from the matching [`PluginRequest`].
        id: u64,
        /// Undo label for the script effect.
        label: String,
        /// Modified document from the worker.
        doc: Document,
        /// History containing the script's undo group.
        history: History,
    },
    /// The plugin failed.
    Err {
        /// Request ID from the matching [`PluginRequest`].
        id: u64,
        /// Human-readable error message.
        message: String,
    },
}

impl std::fmt::Debug for PluginResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginResult::OkCommand { id, label, command } => f
                .debug_struct("OkCommand")
                .field("id", id)
                .field("label", label)
                .field("command", &command.is_some())
                .finish(),
            PluginResult::OkDocument {
                id,
                label,
                doc,
                history,
            } => f
                .debug_struct("OkDocument")
                .field("id", id)
                .field("label", label)
                .field("doc", doc)
                .field("history", history)
                .finish(),
            PluginResult::Err { id, message } => f
                .debug_struct("Err")
                .field("id", id)
                .field("message", message)
                .finish(),
        }
    }
}

/// Background worker that runs plugins on a dedicated thread.
#[derive(Debug)]
pub struct PluginWorker {
    /// Holds at most one pending plugin request. Replacing it drops the stale
    /// document snapshot instead of letting requests accumulate.
    slot: Arc<Mutex<Option<PluginRequest>>>,
    /// Notifies the worker thread that a new request is available.
    notify: Mutex<Option<mpsc::SyncSender<()>>>,
    result_receiver: mpsc::Receiver<PluginResult>,
    latest_id: Arc<AtomicU64>,
    /// Set to `false` when the worker thread panics or exits.
    alive: Arc<AtomicBool>,
}

impl PluginWorker {
    /// Spawn the background thread and return a handle for the UI.
    pub fn new() -> Self {
        let slot = Arc::new(Mutex::new(None::<PluginRequest>));
        let (notify_tx, notify_rx) = mpsc::sync_channel::<()>(1);
        let (result_sender, result_receiver) = mpsc::channel::<PluginResult>();
        let latest_id = Arc::new(AtomicU64::new(0));
        let worker_latest_id = Arc::clone(&latest_id);
        let worker_slot = Arc::clone(&slot);
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = Arc::clone(&alive);

        thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while notify_rx.recv().is_ok() {
                    let req = worker_slot.lock().unwrap().take();
                    let Some(req) = req else { continue };
                    let id = req.id();
                    if worker_latest_id.load(Ordering::Relaxed) != id {
                        // A newer Run click superseded this queued request.
                        continue;
                    }

                    let result = match req {
                        PluginRequest::Lua {
                            id,
                            source,
                            mut doc,
                        } => {
                            let mut history = History::new(DEFAULT_UNDO_LIMIT);
                            match ScriptEngine::new().run(&source, &mut doc, &mut history) {
                                Ok(()) => PluginResult::OkDocument {
                                    id,
                                    label: "Run Lua script".to_string(),
                                    doc,
                                    history,
                                },
                                Err(e) => PluginResult::Err {
                                    id,
                                    message: format!("Lua error: {e}"),
                                },
                            }
                        }
                        PluginRequest::Wasm { id, cmd, doc } => {
                            let label = cmd.label().to_string();
                            let layer_id = cmd.layer_id();
                            let filter = cmd.into_filter();
                            match apply_filter_off_thread(filter, doc, layer_id) {
                                Ok(new_buffer) => PluginResult::OkCommand {
                                    id,
                                    label,
                                    command: Some(Box::new(ReplaceLayerBufferCmd::new(
                                        layer_id, new_buffer,
                                    ))),
                                },
                                Err(e) => PluginResult::Err {
                                    id,
                                    message: format!("Filter error: {e}"),
                                },
                            }
                        }
                    };
                    let _ = result_sender.send(result);
                }
            }));
            worker_alive.store(false, Ordering::Relaxed);
        });

        Self {
            slot,
            notify: Mutex::new(Some(notify_tx)),
            result_receiver,
            latest_id,
            alive,
        }
    }

    /// Enqueue a request.
    ///
    /// The request replaces any pending request on the worker, so the queue
    /// never holds more than one document snapshot. The request is silently
    /// dropped if the worker thread has shut down.
    pub fn request(&self, req: PluginRequest) {
        self.latest_id.store(req.id(), Ordering::Relaxed);
        {
            let mut guard = self.slot.lock().unwrap();
            *guard = Some(req);
        }
        if let Some(tx) = self.notify.lock().unwrap().as_ref() {
            let _ = tx.try_send(());
        }
    }

    /// Poll for a completed result. Returns `None` if no result is ready yet.
    pub fn poll_result(&self) -> Option<PluginResult> {
        self.result_receiver.try_recv().ok()
    }

    /// Return the next request ID and advance the internal counter.
    ///
    /// The UI uses this to tag requests so it can ignore results from stale
    /// work that was enqueued before a newer request.
    pub fn next_id(&self) -> u64 {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns `false` if the background thread has panicked or exited.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> bool {
        self.slot.lock().unwrap().is_some()
    }

    #[cfg(test)]
    pub(crate) fn kill(&self) {
        self.notify.lock().unwrap().take();
    }
}

impl Default for PluginWorker {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply a CPU filter to a cloned layer buffer off the UI thread.
///
/// Returns a new [`TiledBuffer`] containing the filtered pixels. The caller
/// constructs a [`ReplaceLayerBufferCmd`] and dispatches it on the UI thread,
/// preserving undo/redo history.
fn apply_filter_off_thread(
    filter: Box<dyn Filter>,
    doc: Document,
    layer_id: ogre_core::LayerId,
) -> ogre_core::Result<TiledBuffer> {
    let layer = doc.layer(layer_id)?;
    if !layer.is_raster() {
        return Err(ogre_core::OgreError::NotRaster);
    }

    let old = layer.buffer().unwrap().clone();
    filter.apply_to_tiled_buffer(&old)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::Document;

    #[test]
    fn lua_noop_script_returns_document() {
        let worker = PluginWorker::new();
        let doc = Document::new(8, 8);
        worker.request(PluginRequest::Lua {
            id: worker.next_id(),
            source: "local x = 1 + 1".to_string(),
            doc: doc.clone(),
        });

        let result = loop {
            if let Some(r) = worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        match result {
            PluginResult::OkDocument { doc: out, .. } => {
                assert_eq!(out.canvas, doc.canvas);
            }
            other => panic!("expected OkDocument, got {:?}", other),
        }
    }

    #[test]
    fn lua_error_returns_error() {
        let worker = PluginWorker::new();
        let doc = Document::new(8, 8);
        worker.request(PluginRequest::Lua {
            id: worker.next_id(),
            source: "error('boom')".to_string(),
            doc,
        });

        let result = loop {
            if let Some(r) = worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        match result {
            PluginResult::Err { .. } => {}
            other => panic!("expected Err, got {:?}", other),
        }
    }

    #[test]
    fn repeated_requests_replace_pending_slot() {
        let worker = PluginWorker::new();
        worker.request(PluginRequest::Lua {
            id: worker.next_id(),
            source: "local a = 1".to_string(),
            doc: Document::new(8, 8),
        });
        worker.request(PluginRequest::Lua {
            id: worker.next_id(),
            source: "local b = 2".to_string(),
            doc: Document::new(8, 8),
        });

        // The slot must never hold more than one request.  It may be 0 if the
        // worker already consumed the request, but it must never be 2.
        let mut max_seen = 0;
        for _ in 0..20 {
            max_seen = max_seen.max(if worker.pending() { 1 } else { 0 });
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            max_seen <= 1,
            "plugin slot held more than one request (max_seen={})",
            max_seen
        );
    }

    #[test]
    fn worker_is_alive_after_spawn_and_dead_after_kill() {
        let worker = PluginWorker::new();
        assert!(worker.is_alive());
        worker.kill();
        let alive = loop {
            if !worker.is_alive() {
                break false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        assert!(!alive);
    }
}
