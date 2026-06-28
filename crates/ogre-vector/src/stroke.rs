// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Stroke geometry: convert raw input samples into a smoothed, variable-width
//! vector path.

use kurbo::{BezPath, CubicBez, ParamCurve, ParamCurveArclen, PathEl, Point, Shape};
use ogre_core::{BrushSettings, InputSample};

/// A single smoothed piece of a brush stroke.
///
/// The contained [`BezPath`] is always a single cubic Bézier segment.  Width
/// varies linearly from `start_width` to `end_width` along the segment, so a
/// renderer can expand it into a variable-thickness outline.
#[derive(Clone, Debug, PartialEq)]
pub struct StrokeSegment {
    /// Cubic Bézier path for this piece of the stroke.
    pub path: BezPath,
    /// Brush width at the start of the segment, in pixels.
    pub start_width: f32,
    /// Brush width at the end of the segment, in pixels.
    pub end_width: f32,
}

/// Incremental builder that turns raw input samples into smoothed stroke
/// segments.
///
/// Samples are smoothed with a Catmull-Rom spline whose endpoints are mirrored
/// so that a two-point stroke still produces a valid straight segment.
///
/// # Incremental emission
///
/// As samples arrive, the segment ending at the most recent sample cannot be
/// finalised because its outgoing tangent is not yet known.  Callers can use
/// [`StrokeBuilder::finalized_since`] to read the segments whose neighbouring
/// samples are all known, and [`StrokeBuilder::preview_segment`] to obtain the
/// trailing provisional segment for live preview.
#[derive(Clone, Debug)]
pub struct StrokeBuilder {
    settings: BrushSettings,
    samples: Vec<InputSample>,
}

impl StrokeBuilder {
    /// Create a new builder with the given brush settings.
    pub fn new(settings: BrushSettings) -> Self {
        Self {
            settings,
            samples: Vec::new(),
        }
    }

    /// Add a sample to the stroke.
    pub fn append(&mut self, sample: InputSample) {
        self.samples.push(sample);
    }

    /// Return the full smoothed path as a sequence of segments.
    ///
    /// This includes both finalized segments and the trailing provisional
    /// segment (if any).  It is useful for tests and for committing a completed
    /// stroke.
    pub fn segments(&self) -> Vec<StrokeSegment> {
        let n = self.samples.len();
        if n < 2 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(n - 1);
        for i in 0..n - 1 {
            out.push(self.segment_at(i));
        }
        out
    }

    /// Return the trailing provisional segment, if any.
    ///
    /// This is the segment from the second-most-recent sample to the most
    /// recent sample.  It becomes a straight line when there are only two
    /// samples; once a third sample arrives it is replaced by a smooth curve
    /// and a new provisional segment is created.
    pub fn preview_segment(&self) -> Option<StrokeSegment> {
        let n = self.samples.len();
        if n < 2 {
            return None;
        }
        Some(self.segment_at(n - 2))
    }

    /// Clear all samples.
    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Number of samples currently stored.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// True if no samples have been appended yet.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Number of finalized segments currently available.
    pub fn finalized_count(&self) -> usize {
        // With n samples there are n-1 total segments.  The last segment is
        // provisional, so n-2 segments are finalized.  For n < 3 this is zero.
        self.samples.len().saturating_sub(2)
    }

    /// Return finalized segments in the index range `[start, count)` without
    /// advancing the internal emission cursor. Used by callers that cache
    /// segments incrementally through an immutable borrow.
    pub fn finalized_since(&self, start: usize) -> Vec<StrokeSegment> {
        let count = self.finalized_count();
        if start >= count {
            return Vec::new();
        }
        (start..count).map(|i| self.segment_at(i)).collect()
    }

    fn segment_at(&self, i: usize) -> StrokeSegment {
        let n = self.samples.len();
        debug_assert!(i + 1 < n, "segment index out of range");

        let p0 = self.samples[i];
        let p1 = self.samples[i + 1];

        let prev = if i == 0 {
            mirror_endpoint(p0.pos, p1.pos)
        } else {
            self.samples[i - 1].pos
        };
        let next = if i + 2 >= n {
            mirror_endpoint(p1.pos, p0.pos)
        } else {
            self.samples[i + 2].pos
        };

        let seg = cubic_from_catmull_rom(p0.pos, p1.pos, prev, next);
        StrokeSegment {
            path: seg.into_path(1.0),
            start_width: self.settings.width_at_pressure(p0.pressure),
            end_width: self.settings.width_at_pressure(p1.pressure),
        }
    }
}

/// Resample a smoothed stroke into [`InputSample`] points suitable for
/// rasterization.
///
/// Points are spaced along each cubic Bézier segment at roughly
/// `settings.step_distance()` intervals, so a downstream rasterizer that stamps
/// along straight segments between them will closely follow the smoothed path.
/// Width and pressure are interpolated linearly along each segment.
pub fn sample_stroke(segments: &[StrokeSegment], settings: &BrushSettings) -> Vec<InputSample> {
    let settings = settings.sanitised();
    let step = settings.step_distance();
    let mut out = Vec::new();
    let mut first = true;

    for seg in segments {
        let [PathEl::MoveTo(a), PathEl::CurveTo(b, c, d)] = seg.path.elements() else {
            continue;
        };
        let cubic = CubicBez::new(*a, *b, *c, *d);
        let arc_len = cubic.arclen(1e-3) as f32;
        if arc_len <= 0.0 {
            continue;
        }

        let n = ((arc_len / step).ceil() as usize).max(1);
        for i in 0..=n {
            if i == 0 && !first {
                continue;
            }
            let t = i as f64 / n as f64;
            let p = cubic.eval(t);
            let width = seg.start_width + (seg.end_width - seg.start_width) * t as f32;
            let pressure = if settings.pressure_size && settings.size > 0.0 {
                (width / settings.size).clamp(0.0, 1.0)
            } else {
                1.0
            };
            out.push(InputSample::with_pressure(
                glam::Vec2::new(p.x as f32, p.y as f32),
                pressure,
            ));
        }
        first = false;
    }

    out
}

fn mirror_endpoint(first: glam::Vec2, second: glam::Vec2) -> glam::Vec2 {
    first + (first - second)
}

fn cubic_from_catmull_rom(
    p0: glam::Vec2,
    p1: glam::Vec2,
    prev: glam::Vec2,
    next: glam::Vec2,
) -> CubicBez {
    let to_kurbo = |v: glam::Vec2| Point::new(v.x as f64, v.y as f64);
    let a = to_kurbo(p0);
    let b = to_kurbo(p1);
    let c1 = to_kurbo(p0 + (p1 - prev) / 6.0);
    let c2 = to_kurbo(p1 - (next - p0) / 6.0);
    CubicBez::new(a, c1, c2, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec2;
    use kurbo::PathEl;

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    fn cubic_points(path: &BezPath) -> [Point; 4] {
        match path.elements() {
            [PathEl::MoveTo(p0), PathEl::CurveTo(p1, p2, p3)] => [*p0, *p1, *p2, *p3],
            _ => panic!("expected a single cubic segment"),
        }
    }

    #[test]
    fn empty_builder_has_no_segments() {
        let b = StrokeBuilder::new(BrushSettings::default());
        assert!(b.segments().is_empty());
        assert!(b.is_empty());
        assert!(b.preview_segment().is_none());
        assert!(b.finalized_since(0).is_empty());
    }

    #[test]
    fn single_sample_has_no_segments() {
        let mut b = StrokeBuilder::new(BrushSettings::default());
        b.append(InputSample::new(Vec2::new(10.0, 20.0)));
        assert!(b.segments().is_empty());
        assert!(b.preview_segment().is_none());
    }

    #[test]
    fn two_point_input_is_straight_tapered_segment() {
        let settings = BrushSettings {
            size: 20.0,
            pressure_size: true,
            ..Default::default()
        };

        let mut b = StrokeBuilder::new(settings);
        b.append(InputSample::with_pressure(Vec2::new(0.0, 0.0), 0.5));
        b.append(InputSample::with_pressure(Vec2::new(10.0, 0.0), 1.0));

        let segs = b.segments();
        assert_eq!(segs.len(), 1);

        let seg = &segs[0];
        assert!((seg.start_width - 10.0).abs() < f32::EPSILON);
        assert!((seg.end_width - 20.0).abs() < f32::EPSILON);

        let [p0, p1, p2, p3] = cubic_points(&seg.path);

        assert!(approx_eq(p0.x, 0.0, 1e-6));
        assert!(approx_eq(p0.y, 0.0, 1e-6));
        assert!(approx_eq(p3.x, 10.0, 1e-6));
        assert!(approx_eq(p3.y, 0.0, 1e-6));

        // A straight horizontal line has control points at the 1/3 and 2/3 marks.
        assert!(approx_eq(p1.x, 10.0 / 3.0, 1e-6));
        assert!(approx_eq(p1.y, 0.0, 1e-6));
        assert!(approx_eq(p2.x, 20.0 / 3.0, 1e-6));
        assert!(approx_eq(p2.y, 0.0, 1e-6));

        // With only two samples the single segment is provisional, not finalized.
        assert!(b.finalized_since(0).is_empty());
        assert!(b.preview_segment().is_some());
    }

    #[test]
    fn three_point_input_produces_two_segments() {
        let mut b = StrokeBuilder::new(BrushSettings::default());
        b.append(InputSample::new(Vec2::new(0.0, 0.0)));
        b.append(InputSample::new(Vec2::new(10.0, 0.0)));
        b.append(InputSample::new(Vec2::new(10.0, 10.0)));
        assert_eq!(b.segments().len(), 2);
    }

    #[test]
    fn smoothing_changes_first_segment_when_third_point_arrives() {
        let mut b = StrokeBuilder::new(BrushSettings::default());
        b.append(InputSample::new(Vec2::new(0.0, 0.0)));
        b.append(InputSample::new(Vec2::new(10.0, 0.0)));

        let first = b.preview_segment().unwrap();

        b.append(InputSample::new(Vec2::new(10.0, 10.0)));
        let finalized = b.finalized_since(0);
        assert_eq!(finalized.len(), 1);

        // The start/end positions stay the same, but the control points must
        // move to introduce the curve through the corner.
        assert_eq!(first.start_width, finalized[0].start_width);
        assert_eq!(first.end_width, finalized[0].end_width);
        assert_ne!(first.path, finalized[0].path);
    }

    #[test]
    fn finalized_since_drains_only_new_segments() {
        // Mirrors the UI's incremental cursor: advance `last` past what was read.
        let mut b = StrokeBuilder::new(BrushSettings::default());
        let mut last = 0;
        b.append(InputSample::new(Vec2::new(0.0, 0.0)));
        b.append(InputSample::new(Vec2::new(10.0, 0.0)));
        assert!(b.finalized_since(last).is_empty());

        b.append(InputSample::new(Vec2::new(20.0, 0.0)));
        let first_batch = b.finalized_since(last);
        assert_eq!(first_batch.len(), 1);
        last = b.finalized_count();

        b.append(InputSample::new(Vec2::new(30.0, 0.0)));
        let second_batch = b.finalized_since(last);
        assert_eq!(second_batch.len(), 1);
        last = b.finalized_count();

        // No new finalized segments until another sample arrives.
        assert!(b.finalized_since(last).is_empty());
    }

    #[test]
    fn clear_resets_sample_state() {
        let mut b = StrokeBuilder::new(BrushSettings::default());
        b.append(InputSample::new(Vec2::new(0.0, 0.0)));
        b.append(InputSample::new(Vec2::new(10.0, 0.0)));
        b.append(InputSample::new(Vec2::new(20.0, 0.0)));
        assert_eq!(b.finalized_since(0).len(), 1);

        b.clear();
        assert!(b.is_empty());
        assert!(b.finalized_since(0).is_empty());
        assert!(b.preview_segment().is_none());
    }
}
