// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Tests for the core error type.

use ogre_core::{LayerId, OgreError, Result};

#[test]
fn error_variants_display_correctly() {
    let id = LayerId::default();
    let cases: Vec<(OgreError, &str)> = vec![
        (OgreError::LayerNotFound(id), "layer LayerId("),
        (OgreError::LayerLocked(id), "layer LayerId("),
        (
            OgreError::NotRaster,
            "operation requires a raster layer, got a group",
        ),
        (OgreError::EmptySelection, "selection is empty"),
    ];
    for (err, want) in cases {
        let msg = format!("{err}");
        assert!(
            msg.contains(want),
            "expected error message to contain {want:?}, got {msg:?}"
        );
    }
}

#[test]
fn result_type_alias_compiles() -> Result<()> {
    // The alias should be usable as a regular Result.
    let ok: Result<i32> = Ok(42);
    assert_eq!(ok?, 42);
    Ok(())
}
