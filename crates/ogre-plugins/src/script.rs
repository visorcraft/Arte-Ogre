// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Safe Lua scripting host for Arte Ogre.
//!
//! [`ScriptEngine`] runs Lua snippets against a [`Document`] and records every
//! generated edit as a single undo/redo group via [`History`]. All mutations go
//! through real `ogre-core` [`Command`] objects, so scripts obey the same
//! single-mutation-path invariant as the rest of the editor.

use std::cell::RefCell;
use std::rc::Rc;

use ogre_core::{
    AddRasterLayerCmd, BatchCmd, Command, CopyToNewLayerCmd, CutToNewLayerCmd, Document, History,
    Rect, Selection, SelectionMode, SetSelectionCmd,
};

/// Errors that can occur while running a Lua script.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// An error from the Lua runtime (syntax, runtime, or callback failure).
    #[error("lua runtime error: {0}")]
    Lua(#[from] mlua::Error),
    /// An error from the document/command layer.
    #[error("document error: {0}")]
    Document(#[from] ogre_core::OgreError),
}

/// Memory cap for a script's Lua VM, bounding a hostile/runaway allocation.
const LUA_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// Instruction cap for a script's Lua VM, bounding a hostile/runaway loop.
const LUA_INSTRUCTION_LIMIT: u32 = 10_000_000;

/// Build a Lua VM that is safe to expose to untrusted scripts.
///
/// `mlua`'s `ALL_SAFE` only excludes the memory-*unsafe* `debug`/`ffi` modules;
/// it still loads `os`, `io`, and `package`, which give a script
/// `os.execute`/`io.open`/`require` — full host filesystem and process access.
/// We instead load only capability-free libraries and then null out the
/// code-loading globals that the always-present base library provides.
fn sandboxed_lua() -> Result<mlua::Lua, mlua::Error> {
    use mlua::{HookTriggers, Lua, LuaOptions, StdLib};

    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8,
        LuaOptions::default(),
    )?;

    // The base library is always loaded and carries capability-bearing globals
    // (`os`/`io`/`package` are absent above, but `load`/`dofile`/… remain).
    let globals = lua.globals();
    for name in [
        "os",
        "io",
        "package",
        "require",
        "dofile",
        "loadfile",
        "load",
        "loadstring",
        "collectgarbage",
        "debug",
    ] {
        globals.set(name, mlua::Value::Nil)?;
    }

    // Bound the VM's memory so a hostile script cannot OOM the host. Ignored if
    // the build lacks a custom allocator.
    let _ = lua.set_memory_limit(LUA_MEMORY_LIMIT);

    // Bound the VM's executed instructions so a hostile script cannot pin the
    // worker thread forever. The hook is installed before any user code runs.
    lua.set_hook(
        HookTriggers::new().every_nth_instruction(LUA_INSTRUCTION_LIMIT),
        move |_, _| {
            Err(mlua::Error::runtime(
                "Lua script exceeded instruction limit",
            ))
        },
    )?;
    Ok(lua)
}

/// Host for running Lua scripts that drive `ogre-core` commands.
#[derive(Debug, Default)]
pub struct ScriptEngine;

impl ScriptEngine {
    /// Create a new scripting engine.
    pub fn new() -> Self {
        Self
    }

    /// Run a Lua script against `doc` and return the generated commands.
    ///
    /// The commands are applied to `doc` as the script runs so that later
    /// callbacks see the effects of earlier ones, but they are not pushed onto
    /// any history. On failure the commands are undone and `doc` is restored.
    ///
    /// The caller can push the returned commands onto its own history, which
    /// keeps prior undo/redo state intact.
    pub fn run_collect(
        &self,
        source: &str,
        doc: &mut Document,
    ) -> Result<Vec<Box<dyn Command>>, ScriptError> {
        let lua = sandboxed_lua()?;

        // Shared context for the scoped callbacks. The context holds the mutable
        // document reference and the list of commands already applied by the
        // script. It is dropped before we touch `doc` again outside the scope.
        struct ScriptContext<'a> {
            doc: &'a mut Document,
            applied: Vec<Box<dyn Command>>,
        }

        let ctx = Rc::new(RefCell::new(ScriptContext {
            doc,
            applied: Vec::new(),
        }));

        let result = lua.scope(|scope| {
            let set_selection_rect = {
                let ctx = Rc::clone(&ctx);
                scope.create_function_mut(move |_, (x, y, w, h): (i32, i32, i32, i32)| {
                    let mut ctx = ctx.borrow_mut();
                    // Args are (x, y, width, height). Reject negative sizes
                    // rather than casting them to enormous `u32` values, and
                    // clamp the rectangle to the canvas so a script cannot
                    // drive downstream ops with an absurdly large selection.
                    if w < 0 || h < 0 {
                        return Err(mlua::Error::RuntimeError(
                            "set_selection_rect: width and height must be non-negative".to_string(),
                        ));
                    }
                    let requested = Rect::new(x, y, w as u32, h as u32);
                    let rect = requested
                        .intersect(ctx.doc.canvas)
                        .unwrap_or_else(|| Rect::new(x, y, 0, 0));
                    let mut cmd =
                        SetSelectionCmd::with_mode(Selection::rect(rect), SelectionMode::Replace);
                    cmd.apply(ctx.doc)
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                    ctx.applied.push(Box::new(cmd));
                    Ok(())
                })?
            };

            let cut_to_new_layer = {
                let ctx = Rc::clone(&ctx);
                scope.create_function_mut(move |_, ()| {
                    let mut ctx = ctx.borrow_mut();
                    let source = ctx
                        .doc
                        .active
                        .ok_or_else(|| mlua::Error::RuntimeError("no active layer".to_string()))?;
                    let mut cmd = CutToNewLayerCmd::new(source, ctx.doc.selection.clone());
                    cmd.apply(ctx.doc)
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                    ctx.applied.push(Box::new(cmd));
                    Ok(())
                })?
            };

            let copy_to_new_layer = {
                let ctx = Rc::clone(&ctx);
                scope.create_function_mut(move |_, ()| {
                    let mut ctx = ctx.borrow_mut();
                    let source = ctx
                        .doc
                        .active
                        .ok_or_else(|| mlua::Error::RuntimeError("no active layer".to_string()))?;
                    let mut cmd = CopyToNewLayerCmd::new(source, ctx.doc.selection.clone());
                    cmd.apply(ctx.doc)
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                    ctx.applied.push(Box::new(cmd));
                    Ok(())
                })?
            };

            let add_raster_layer = {
                let ctx = Rc::clone(&ctx);
                scope.create_function_mut(move |_, name: String| {
                    let mut ctx = ctx.borrow_mut();
                    let mut cmd = AddRasterLayerCmd::new(name);
                    cmd.apply(ctx.doc)
                        .map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                    ctx.applied.push(Box::new(cmd));
                    Ok(())
                })?
            };

            lua.globals()
                .set("set_selection_rect", set_selection_rect)?;
            lua.globals().set("cut_to_new_layer", cut_to_new_layer)?;
            lua.globals().set("copy_to_new_layer", copy_to_new_layer)?;
            lua.globals().set("add_raster_layer", add_raster_layer)?;

            lua.load(source).exec()
        });

        // Extract the applied commands and release the document borrow before
        // touching `doc` again.
        let applied = {
            let mut ctx_ref = ctx.borrow_mut();
            std::mem::take(&mut ctx_ref.applied)
        };
        drop(ctx);

        match result {
            Err(e) => {
                // Roll back every command that succeeded before the failure.
                for mut cmd in applied.into_iter().rev() {
                    cmd.undo(doc);
                }
                Err(ScriptError::Lua(e))
            }
            Ok(()) => Ok(applied),
        }
    }

    /// Run a Lua script against `doc`, recording all generated commands as a
    /// single undo group pushed onto `history`.
    ///
    /// If the script fails after some commands were already applied, those
    /// commands are undone in reverse order before the error is returned,
    /// leaving `doc` in its original state.
    pub fn run(
        &self,
        source: &str,
        doc: &mut Document,
        history: &mut History,
    ) -> Result<(), ScriptError> {
        let applied = self.run_collect(source, doc)?;
        if applied.is_empty() {
            return Ok(());
        }
        let mut batch = BatchCmd::new("Run Lua script");
        for cmd in applied {
            batch.push(cmd);
        }
        history.push_applied(doc, Box::new(batch));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ogre_core::{CutToNewLayerCmd, Rgba32F, Selection, SetSelectionCmd};

    fn make_doc_with_pixel() -> Document {
        let mut doc = Document::new(20, 20);
        let bg = doc.add_raster_layer("Background");
        doc.layer_mut(bg).unwrap().buffer_mut().unwrap().set_pixel(
            ogre_core::IVec2::new(5, 5),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0),
        );
        doc.active = Some(bg);
        doc
    }

    #[test]
    fn lua_cut_to_new_layer_matches_native_sequence() {
        let mut scripted_doc = make_doc_with_pixel();
        let mut scripted_history = History::new(0);
        let engine = ScriptEngine::new();
        engine
            .run(
                r#"
                    set_selection_rect(0, 0, 10, 10)
                    cut_to_new_layer()
                "#,
                &mut scripted_doc,
                &mut scripted_history,
            )
            .unwrap();

        let mut native_doc = make_doc_with_pixel();
        let bg = native_doc.active.unwrap();
        SetSelectionCmd::with_mode(
            Selection::rect(Rect::new(0, 0, 10, 10)),
            SelectionMode::Replace,
        )
        .apply(&mut native_doc)
        .unwrap();
        CutToNewLayerCmd::new(bg, native_doc.selection.clone())
            .apply(&mut native_doc)
            .unwrap();

        assert_eq!(scripted_doc, native_doc);
        assert_eq!(scripted_doc.order.len(), native_doc.order.len());
        assert_eq!(scripted_doc.active, native_doc.active);

        let scripted_bg = scripted_doc.order[0];
        let native_bg = native_doc.order[0];
        assert_eq!(
            scripted_doc
                .layer(scripted_bg)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(ogre_core::IVec2::new(5, 5)),
            native_doc
                .layer(native_bg)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(ogre_core::IVec2::new(5, 5))
        );

        let scripted_new = scripted_doc.active.unwrap();
        let native_new = native_doc.active.unwrap();
        assert_eq!(
            scripted_doc
                .layer(scripted_new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(ogre_core::IVec2::new(5, 5)),
            native_doc
                .layer(native_new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(ogre_core::IVec2::new(5, 5))
        );
        assert_eq!(
            scripted_doc
                .layer(scripted_new)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(ogre_core::IVec2::new(5, 5)),
            Rgba32F::new(1.0, 0.0, 0.0, 1.0)
        );
    }

    #[test]
    fn failed_script_rolls_back_applied_commands() {
        let mut doc = make_doc_with_pixel();
        let original_pixel = doc
            .layer(doc.active.unwrap())
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(ogre_core::IVec2::new(5, 5));
        let mut history = History::new(0);
        let engine = ScriptEngine::new();

        let result = engine.run(
            r#"
                set_selection_rect(0, 0, 10, 10)
                error("boom")
            "#,
            &mut doc,
            &mut history,
        );

        assert!(result.is_err());
        assert!(doc.selection.is_empty());
        assert_eq!(
            doc.layer(doc.active.unwrap())
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(ogre_core::IVec2::new(5, 5)),
            original_pixel
        );
        assert_eq!(history.undo_len(), 0);
    }

    #[test]
    fn lua_script_forms_single_undo_group() {
        let mut doc = Document::new(100, 100);
        doc.add_raster_layer("Background");
        let mut history = History::new(0);
        let engine = ScriptEngine::new();

        engine
            .run(
                r#"
                    add_raster_layer("From Lua")
                    set_selection_rect(0, 0, 5, 5)
                "#,
                &mut doc,
                &mut history,
            )
            .unwrap();

        assert_eq!(history.undo_len(), 1);
        history.undo(&mut doc);
        assert_eq!(doc.order.len(), 1);
        assert!(doc.selection.is_empty());
    }

    #[test]
    fn lua_sandbox_removes_dangerous_globals() {
        let mut doc = Document::new(8, 8);
        doc.add_raster_layer("bg");
        let mut history = History::new(0);
        let engine = ScriptEngine::new();
        // A hostile script must not be able to reach the filesystem, run
        // processes, or load native code. These globals must all be nil.
        let script = r#"
            assert(os == nil, "os must be nil")
            assert(io == nil, "io must be nil")
            assert(package == nil, "package must be nil")
            assert(require == nil, "require must be nil")
            assert(dofile == nil, "dofile must be nil")
            assert(loadfile == nil, "loadfile must be nil")
            assert(load == nil, "load must be nil")
            assert(coroutine == nil, "coroutine must be nil")
            -- the safe libraries we do expose must still work
            assert(type(string.upper) == "function")
            assert(math.floor(1.5) == 1)
        "#;
        engine
            .run(script, &mut doc, &mut history)
            .expect("sandboxed script should run with safe libs and no dangerous globals");
    }

    #[test]
    fn set_selection_rect_rejects_negative_size() {
        let mut doc = make_doc_with_pixel();
        let mut history = History::new(0);
        let engine = ScriptEngine::new();
        let result = engine.run("set_selection_rect(0, 0, -1, -1)", &mut doc, &mut history);
        assert!(result.is_err(), "negative width/height must be rejected");
        assert!(
            doc.selection.is_empty(),
            "a rejected selection must not be applied"
        );
        assert_eq!(history.undo_len(), 0);
    }

    #[test]
    fn empty_script_pushes_no_history() {
        let mut doc = Document::new(10, 10);
        doc.add_raster_layer("bg");
        let mut history = History::new(0);
        let engine = ScriptEngine::new();
        engine
            .run("local x = 1 + 1", &mut doc, &mut history)
            .unwrap();
        assert_eq!(
            history.undo_len(),
            0,
            "a no-op script must not create an undo entry"
        );
    }

    #[test]
    fn runaway_script_hits_instruction_limit() {
        let mut doc = make_doc_with_pixel();
        let original = doc.clone();
        let mut history = History::new(0);
        let engine = ScriptEngine::new();

        let result = engine.run("while true do end", &mut doc, &mut history);

        assert!(result.is_err(), "runaway script must be aborted");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("instruction limit"),
            "error must mention instruction limit, got: {err}"
        );
        assert_eq!(doc, original, "document must be unchanged");
        assert_eq!(history.undo_len(), 0, "no undo entry must be created");
    }
}
