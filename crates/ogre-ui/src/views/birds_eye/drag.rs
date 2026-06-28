// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC
//! Pure drag-and-drop math for Bird's Eye layer rows.
//!
//! Rows are laid out left-to-right in bottom-to-top z-order. These helpers
//! translate a pointer position into an insertion gap and, for same-document
//! reorders, into the final z-index after the dragged layer is removed.

/// Return the insertion gap for a pointer in a left-to-right row of thumbnail
/// rectangles. The result is in `0..=rects.len()`:
///
/// - pointer left of the first center -> `0`
/// - pointer between two centers -> the gap between them
/// - pointer right of the last center -> `rects.len()`
pub fn gap_index_from_pointer_x(rects: &[egui::Rect], pointer_x: f32) -> usize {
    rects.iter().filter(|r| r.center().x < pointer_x).count()
}

/// Convert an insertion `gap_index` in the displayed row (which still includes
/// the dragged item) into the final z-index after the dragged item is removed
/// from `original_index`. The result is in `0..len-1` for non-empty rows.
///
/// Cross-document inserts do not use this function: their target row never loses
/// an item first, so they insert at the raw gap (`0..=target_len`).
pub fn same_document_gap_to_final_index(
    len: usize,
    original_index: usize,
    gap_index: usize,
) -> usize {
    if len <= 1 {
        return 0;
    }
    // Removing the dragged item shifts every gap above it down by one.
    let final_index = if gap_index > original_index {
        gap_index - 1
    } else {
        gap_index
    };
    final_index.min(len - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(centers: &[f32]) -> Vec<egui::Rect> {
        centers
            .iter()
            .map(|&cx| egui::Rect::from_center_size(egui::pos2(cx, 0.0), egui::vec2(20.0, 20.0)))
            .collect()
    }

    #[test]
    fn pointer_before_first_center_is_gap_zero() {
        let rects = row(&[10.0, 40.0, 70.0]);
        assert_eq!(gap_index_from_pointer_x(&rects, 0.0), 0);
        assert_eq!(gap_index_from_pointer_x(&rects, 9.9), 0);
    }

    #[test]
    fn pointer_between_centers_is_corresponding_gap() {
        let rects = row(&[10.0, 40.0, 70.0]);
        assert_eq!(gap_index_from_pointer_x(&rects, 25.0), 1);
        assert_eq!(gap_index_from_pointer_x(&rects, 55.0), 2);
    }

    #[test]
    fn pointer_after_last_center_is_gap_len() {
        let rects = row(&[10.0, 40.0, 70.0]);
        assert_eq!(gap_index_from_pointer_x(&rects, 1000.0), 3);
    }

    #[test]
    fn drag_first_item_after_last_yields_top_index() {
        // 4 items, drag index 0, drop past the last center -> gap 4.
        let final_index = same_document_gap_to_final_index(4, 0, 4);
        assert_eq!(final_index, 3, "first dragged to the end is the top index");
    }

    #[test]
    fn drag_last_item_before_first_yields_bottom_index() {
        // 4 items, drag index 3, drop before the first center -> gap 0.
        let final_index = same_document_gap_to_final_index(4, 3, 0);
        assert_eq!(
            final_index, 0,
            "last dragged to the start is the bottom index"
        );
    }

    #[test]
    fn cross_document_empty_target_returns_gap_zero() {
        let rects: Vec<egui::Rect> = Vec::new();
        assert_eq!(gap_index_from_pointer_x(&rects, 123.0), 0);
    }

    #[test]
    fn cross_document_after_last_thumbnail_returns_gap_len() {
        let rects = row(&[10.0, 40.0]);
        assert_eq!(gap_index_from_pointer_x(&rects, 500.0), 2);
    }

    #[test]
    fn same_document_noop_gap_keeps_position() {
        // Dropping into the gap just before the dragged item is a no-op.
        assert_eq!(same_document_gap_to_final_index(4, 2, 2), 2);
        // Dropping into the gap just after the dragged item is also a no-op.
        assert_eq!(same_document_gap_to_final_index(4, 2, 3), 2);
    }
}
