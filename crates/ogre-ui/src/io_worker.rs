// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Background worker for blocking save/export/import I/O.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;

use ogre_core::{Document, Rect};

/// Default SVG import options used when the user has not been shown the import
/// options dialog (e.g., command-line open or non-SVG files).
pub use ogre_io::svg::SvgImportOptions;

/// Kind of export requested from the UI thread.
#[derive(Debug, Clone)]
pub enum ExportKind {
    /// Native OpenRaster archive.
    Ora,
    /// Flat raster image with options and canvas region.
    Raster {
        /// Document-space region to export.
        canvas: Rect,
        /// Raster encoding options.
        options: ogre_io::raster::ExportOptions,
    },
    /// Flat SVG with an embedded PNG of the composited canvas.
    Svg,
    /// Export each document slice as a separate raster image.
    Slices {
        /// Raster encoding options.
        options: ogre_io::raster::ExportOptions,
    },
}

/// Request sent to the background I/O thread.
#[derive(Debug)]
pub enum IoRequest {
    /// Save the native `.ogre` format.
    Save {
        /// Request ID echoed back in the result so the UI can ignore stale work.
        id: u64,
        /// Document snapshot to save.
        doc: Document,
        /// Destination path.
        path: PathBuf,
    },
    /// Export to `.ora` or a flat raster format.
    Export {
        /// Request ID echoed back in the result so the UI can ignore stale work.
        id: u64,
        /// Document snapshot to export.
        doc: Document,
        /// Destination path.
        path: PathBuf,
        /// Export format and options.
        kind: ExportKind,
    },
    /// Open/import any supported format, producing a new document.
    Open {
        /// Request ID echoed back in the result so the UI can ignore stale work.
        id: u64,
        /// Source path.
        path: PathBuf,
        /// SVG-specific import options. Ignored for non-SVG formats.
        svg_options: SvgImportOptions,
    },
    /// Import an image and add its first raster layer to the current document.
    AddAsLayer {
        /// Request ID echoed back in the result so the UI can ignore stale work.
        id: u64,
        /// Source path.
        path: PathBuf,
    },
}

/// Which kind of I/O operation produced a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IoKind {
    /// Native `.ogre` save.
    Save,
    /// Export to `.ora` or a flat raster format.
    Export,
    /// Open/import any supported format.
    Open,
    /// Import and add as layer.
    AddAsLayer,
}

impl IoRequest {
    fn kind(&self) -> IoKind {
        match self {
            IoRequest::Save { .. } => IoKind::Save,
            IoRequest::Export { .. } => IoKind::Export,
            IoRequest::Open { .. } => IoKind::Open,
            IoRequest::AddAsLayer { .. } => IoKind::AddAsLayer,
        }
    }

    fn id(&self) -> u64 {
        match self {
            IoRequest::Save { id, .. }
            | IoRequest::Export { id, .. }
            | IoRequest::Open { id, .. }
            | IoRequest::AddAsLayer { id, .. } => *id,
        }
    }
}

/// Result delivered back to the UI thread.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum IoResult {
    /// Save completed successfully.
    SaveOk {
        /// Request ID from the matching [`IoRequest`].
        id: u64,
        /// Destination path that was written.
        path: PathBuf,
    },
    /// Export completed successfully.
    ExportOk {
        /// Request ID from the matching [`IoRequest`].
        id: u64,
        /// Destination path that was written.
        path: PathBuf,
    },
    /// Open completed successfully.
    OpenOk {
        /// Request ID from the matching [`IoRequest`].
        id: u64,
        /// Loaded document.
        doc: Document,
        /// Source path that was read.
        path: PathBuf,
    },
    /// Add-as-layer completed successfully.
    AddAsLayerOk {
        /// Request ID from the matching [`IoRequest`].
        id: u64,
        /// Loaded document containing the layer to add.
        doc: Document,
        /// Source path that was read.
        path: PathBuf,
    },
    /// The operation failed.
    Err {
        /// Request ID from the matching [`IoRequest`].
        id: u64,
        /// Which operation failed.
        kind: IoKind,
        /// Path involved in the failure.
        path: PathBuf,
        /// Human-readable error message.
        message: String,
    },
}

/// Tracks the latest request ID for each I/O kind.
///
/// The worker uses this to skip stale queued requests instead of performing
/// redundant, expensive I/O on a document snapshot that the UI has already
/// superseded.
#[derive(Debug)]
struct LatestIds {
    save: AtomicU64,
    export: AtomicU64,
    open: AtomicU64,
    add_as_layer: AtomicU64,
}

impl LatestIds {
    fn new() -> Self {
        Self {
            save: AtomicU64::new(0),
            export: AtomicU64::new(0),
            open: AtomicU64::new(0),
            add_as_layer: AtomicU64::new(0),
        }
    }

    fn update(&self, kind: IoKind, id: u64) {
        let target = match kind {
            IoKind::Save => &self.save,
            IoKind::Export => &self.export,
            IoKind::Open => &self.open,
            IoKind::AddAsLayer => &self.add_as_layer,
        };
        target.store(id, Ordering::Relaxed);
    }

    fn is_current(&self, kind: IoKind, id: u64) -> bool {
        let target = match kind {
            IoKind::Save => &self.save,
            IoKind::Export => &self.export,
            IoKind::Open => &self.open,
            IoKind::AddAsLayer => &self.add_as_layer,
        };
        target.load(Ordering::Relaxed) == id
    }
}

impl Default for LatestIds {
    fn default() -> Self {
        Self::new()
    }
}

/// Background worker that performs blocking I/O on a dedicated thread.
#[derive(Debug)]
pub struct IoWorker {
    /// Holds at most one pending request per [`IoKind`]. Replacing an existing
    /// slot drops the stale document snapshot instead of letting it accumulate.
    slots: Arc<Mutex<HashMap<IoKind, IoRequest>>>,
    /// Notifies the worker thread that a new request is available. Capacity 1
    /// is enough: one wakeup coalesces all slot replacements made while the
    /// worker is busy.
    notify: Mutex<Option<mpsc::SyncSender<()>>>,
    /// Channel used to receive results from the background thread.
    result_receiver: mpsc::Receiver<IoResult>,
    /// Latest request ID per kind, shared with the worker thread.
    latest_ids: Arc<LatestIds>,
    /// Set to `false` when the worker thread panics or exits.
    alive: Arc<AtomicBool>,
}

impl IoWorker {
    /// Spawn the background thread and return a handle for the UI.
    pub fn new() -> Self {
        let slots = Arc::new(Mutex::new(HashMap::<IoKind, IoRequest>::new()));
        let (notify_tx, notify_rx) = mpsc::sync_channel::<()>(1);
        let (result_sender, result_receiver) = mpsc::channel::<IoResult>();
        let latest_ids = Arc::new(LatestIds::new());
        let worker_latest_ids = Arc::clone(&latest_ids);
        let worker_slots = Arc::clone(&slots);
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = Arc::clone(&alive);

        thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                while notify_rx.recv().is_ok() {
                    let requests: Vec<IoRequest> = {
                        let mut guard = worker_slots.lock().unwrap();
                        guard.drain().map(|(_, req)| req).collect()
                    };
                    for req in requests {
                        let kind = req.kind();
                        let id = req.id();
                        if !worker_latest_ids.is_current(kind, id) {
                            // A newer request of the same kind superseded this one.
                            continue;
                        }

                        let result = match req {
                            IoRequest::Save { id, doc, path } => {
                                match ogre_io::ogre::save(&doc, &path) {
                                    Ok(()) => IoResult::SaveOk { id, path },
                                    Err(e) => IoResult::Err {
                                        id,
                                        kind: IoKind::Save,
                                        path,
                                        message: e.to_string(),
                                    },
                                }
                            }
                            IoRequest::Export {
                                id,
                                doc,
                                path,
                                kind,
                            } => {
                                let res = match kind {
                                    ExportKind::Ora => {
                                        ogre_io::interchange::export_ora(&doc, &path)
                                    }
                                    ExportKind::Raster {
                                        canvas,
                                        ref options,
                                    } => {
                                        ogre_io::raster::export_image(&doc, canvas, options, &path)
                                    }
                                    ExportKind::Svg => ogre_io::svg::export_svg(&doc, &path),
                                    ExportKind::Slices { ref options } => {
                                        ogre_io::raster::export_slices(&doc, &path, options)
                                            .map(|_| ())
                                    }
                                };
                                match res {
                                    Ok(()) => IoResult::ExportOk { id, path },
                                    Err(e) => IoResult::Err {
                                        id,
                                        kind: IoKind::Export,
                                        path,
                                        message: e.to_string(),
                                    },
                                }
                            }
                            IoRequest::Open {
                                id,
                                path,
                                svg_options,
                            } => {
                                import_document(id, path, svg_options, IoKind::Open, |doc, path| {
                                    IoResult::OpenOk { id, doc, path }
                                })
                            }
                            IoRequest::AddAsLayer { id, path } => import_document(
                                id,
                                path,
                                SvgImportOptions::default(),
                                IoKind::AddAsLayer,
                                |doc, path| IoResult::AddAsLayerOk { id, doc, path },
                            ),
                        };
                        let _ = result_sender.send(result);
                    }
                }
            }));
            worker_alive.store(false, Ordering::Relaxed);
        });

        Self {
            slots,
            notify: Mutex::new(Some(notify_tx)),
            result_receiver,
            latest_ids,
            alive,
        }
    }

    /// Enqueue a request.
    ///
    /// The request replaces any pending request of the same kind, so the queue
    /// never holds more than one document snapshot per kind. The request is
    /// silently dropped if the worker thread has shut down.
    pub fn request(&self, req: IoRequest) {
        self.latest_ids.update(req.kind(), req.id());
        {
            let mut guard = self.slots.lock().unwrap();
            guard.insert(req.kind(), req);
        }
        if let Some(tx) = self.notify.lock().unwrap().as_ref() {
            // Coalesce wakeups: if a notification is already queued, the worker
            // will drain all slots when it wakes.
            let _ = tx.try_send(());
        }
    }

    /// Poll for a completed result. Returns `None` if no result is ready yet.
    pub fn poll_result(&self) -> Option<IoResult> {
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

    /// Number of pending document snapshots queued on the worker.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    /// Stop the worker thread so tests can observe `is_alive` becoming `false`.
    #[cfg(test)]
    pub(crate) fn kill(&self) {
        self.notify.lock().unwrap().take();
    }
}

impl Default for IoWorker {
    fn default() -> Self {
        Self::new()
    }
}

fn import_document<F>(
    id: u64,
    path: PathBuf,
    svg_options: SvgImportOptions,
    kind: IoKind,
    ok: F,
) -> IoResult
where
    F: FnOnce(Document, PathBuf) -> IoResult,
{
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    let res = match ext.as_str() {
        "ogre" => ogre_io::ogre::load(&path),
        "ora" => ogre_io::interchange::import_ora(&path),
        "psd" => ogre_io::interchange::import_psd(&path),
        "svg" | "svgz" => ogre_io::svg::import_svg_file(&path, svg_options),
        _ => ogre_io::raster::import_image(&path),
    };
    match res {
        Ok(doc) => ok(doc, path),
        Err(e) => IoResult::Err {
            id,
            kind,
            path,
            message: e.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ogre_io_worker_test_{}_{}",
            std::process::id(),
            name
        ))
    }

    #[test]
    fn save_round_trip_off_thread() {
        let worker = IoWorker::new();
        let mut doc = Document::new(8, 8);
        doc.add_raster_layer("bg");
        let path = temp_path("save_round_trip.ogre");
        let _ = std::fs::remove_file(&path);

        let id1 = worker.next_id();
        worker.request(IoRequest::Save {
            id: id1,
            doc: doc.clone(),
            path: path.clone(),
        });

        let result = loop {
            if let Some(r) = worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        match result {
            IoResult::SaveOk { id, path: p } => {
                assert_eq!(id, id1);
                assert_eq!(p, path);
            }
            other => panic!("expected SaveOk, got {:?}", other),
        }

        let id2 = worker.next_id();
        worker.request(IoRequest::Open {
            id: id2,
            path: path.clone(),
            svg_options: SvgImportOptions::default(),
        });
        let result = loop {
            if let Some(r) = worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        match result {
            IoResult::OpenOk {
                id,
                doc: loaded,
                path: p,
            } => {
                assert_eq!(id, id2);
                assert_eq!(p, path);
                assert_eq!(loaded.canvas, doc.canvas);
                assert_eq!(loaded.order.len(), doc.order.len());
            }
            other => panic!("expected OpenOk, got {:?}", other),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_missing_file_returns_error() {
        let worker = IoWorker::new();
        let path = temp_path("definitely_missing.ogre");
        let _ = std::fs::remove_file(&path);

        let id = worker.next_id();
        worker.request(IoRequest::Open {
            id,
            path: path.clone(),
            svg_options: SvgImportOptions::default(),
        });
        let result = loop {
            if let Some(r) = worker.poll_result() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };

        match result {
            IoResult::Err {
                id: rid, path: p, ..
            } => {
                assert_eq!(rid, id);
                assert_eq!(p, path);
            }
            other => panic!("expected Err, got {:?}", other),
        }
    }

    #[test]
    fn repeated_same_kind_requests_replace_pending_slot() {
        let worker = IoWorker::new();
        let path = temp_path("replace.ogre");
        let _ = std::fs::remove_file(&path);

        let id1 = worker.next_id();
        worker.request(IoRequest::Save {
            id: id1,
            doc: Document::new(8, 8),
            path: path.with_extension("a.ogre"),
        });

        // Spin briefly so the first request may or may not be picked up; the
        // important invariant is that at most one Save slot exists.
        for _ in 0..10 {
            assert!(worker.pending_count() <= 1, "queue grew beyond one slot");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let id2 = worker.next_id();
        worker.request(IoRequest::Save {
            id: id2,
            doc: Document::new(8, 8),
            path: path.clone(),
        });

        assert!(worker.pending_count() <= 1, "replacement grew the queue");

        // Wait for the latest save to finish, ignoring any stale result from a
        // request that the worker picked up before being replaced.
        let mut result_id = None;
        while result_id != Some(id2) {
            if let Some(IoResult::SaveOk { id, .. }) = worker.poll_result() {
                result_id = Some(id);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn different_kinds_each_hold_one_slot() {
        let worker = IoWorker::new();
        worker.request(IoRequest::Save {
            id: worker.next_id(),
            doc: Document::new(8, 8),
            path: temp_path("x.ogre"),
        });
        worker.request(IoRequest::Export {
            id: worker.next_id(),
            doc: Document::new(8, 8),
            path: temp_path("x.png"),
            kind: ExportKind::Raster {
                canvas: Rect::new(0, 0, 8, 8),
                options: ogre_io::raster::ExportOptions {
                    format: ogre_io::RasterFormat::Png,
                    quality: None,
                    bit_depth: ogre_io::RasterBitDepth::Eight,
                },
            },
        });
        worker.request(IoRequest::Open {
            id: worker.next_id(),
            path: temp_path("missing.ogre"),
            svg_options: SvgImportOptions::default(),
        });

        // The worker may start consuming immediately, so the exact count is
        // racy; the invariant is it never exceeds the number of kinds.
        for _ in 0..20 {
            assert!(
                worker.pending_count() <= 4,
                "queue grew beyond four kind slots"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn worker_is_alive_after_spawn_and_dead_after_kill() {
        let worker = IoWorker::new();
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
