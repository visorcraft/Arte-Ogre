// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Canvas grid and snap helpers.
//!
//! The grid is purely view-state: it does not affect the document or the undo
//! history. Snap helpers round document-space coordinates to the nearest grid
//! intersection.

use glam::{IVec2, Vec2};

/// Snap a document-space position to the nearest grid intersection.
///
/// A `spacing` of `0` disables snapping and returns the input rounded to the
/// nearest integer pixel.
pub fn snap_to_grid(doc_pos: Vec2, spacing: u32) -> IVec2 {
    if spacing == 0 {
        return IVec2::new(doc_pos.x.round() as i32, doc_pos.y.round() as i32);
    }
    let s = spacing as f32;
    IVec2::new(
        (doc_pos.x / s).round() as i32 * spacing as i32,
        (doc_pos.y / s).round() as i32 * spacing as i32,
    )
}

/// Return the document-space range of grid lines that cover `region`.
///
/// `region` is in document pixels; the returned iterator yields line
/// coordinates at multiples of `spacing` that fall inside the range.
pub fn grid_lines_in_range(min: f32, max: f32, spacing: u32) -> impl Iterator<Item = i32> {
    // A spacing of 0 disables the grid; return an empty range rather than
    // dividing by zero in `div_euclid`.
    let s = spacing.max(1) as i32;
    let (start, end) = if spacing == 0 {
        (1, 0) // empty inclusive range
    } else {
        (
            (min.floor() as i32).div_euclid(s) * s,
            (max.ceil() as i32).div_euclid(s) * s,
        )
    };
    (start..=end).step_by(s as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_to_grid_with_zero_spacing_rounds_to_pixel() {
        assert_eq!(snap_to_grid(Vec2::new(2.3, 5.7), 0), IVec2::new(2, 6));
    }

    #[test]
    fn snap_to_grid_snaps_to_nearest_multiple() {
        assert_eq!(snap_to_grid(Vec2::new(14.0, 23.0), 16), IVec2::new(16, 16));
        // Halfway between grid lines rounds away from zero.
        assert_eq!(snap_to_grid(Vec2::new(8.0, 8.0), 16), IVec2::new(16, 16));
    }

    #[test]
    fn snap_to_grid_handles_negative_coords() {
        assert_eq!(
            snap_to_grid(Vec2::new(-8.0, -24.0), 16),
            IVec2::new(-16, -32)
        );
    }

    #[test]
    fn grid_lines_in_range_covers_visible_region() {
        let lines: Vec<i32> = grid_lines_in_range(10.0, 50.0, 16).collect();
        assert_eq!(lines, vec![0, 16, 32, 48]);
    }

    #[test]
    fn grid_lines_in_range_handles_negative_origin() {
        let lines: Vec<i32> = grid_lines_in_range(-20.0, 20.0, 16).collect();
        assert_eq!(lines, vec![-32, -16, 0, 16]);
    }

    #[test]
    fn grid_lines_in_range_zero_spacing_is_empty_not_panic() {
        // Spacing 0 (grid disabled) must not divide by zero.
        let lines: Vec<i32> = grid_lines_in_range(0.0, 100.0, 0).collect();
        assert!(lines.is_empty());
    }
}
