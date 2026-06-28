// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! Remove a "fake transparency" matte and recover true alpha.
//!
//! Many flattened images fake transparency by compositing the subject over an
//! opaque **matte** — either a solid near-uniform fill (white / grey) or the
//! classic transparency **checkerboard** (two alternating colors). This module
//! detects such a matte from the layer border and lifts the connected
//! background region to real `alpha = 0`, recovering anti-aliased edges
//! *losslessly*: compositing a changed pixel back over the matte color it was
//! lifted from reproduces the original pixel exactly. This is the GIMP
//! "Color to Alpha" unmixing identity.
//!
//! All math runs in the buffer's native straight-alpha, linear-light space —
//! the same space [`crate::composite_document`] blends in — so the round-trip
//! holds against the reference compositor.

use crate::buffer::TiledBuffer;
use crate::coord::{IVec2, Rect};
use crate::pixel::Rgba32F;

/// Default color-distance tolerance for matte detection and removal.
///
/// An RGB Euclidean distance (in the buffer's linear space) below which a pixel
/// counts as pure matte and is cleared to full transparency.
pub const DEFAULT_MATTE_TOLERANCE: f32 = 0.2;

/// Default [`MatteOptions::edge_softness`]: how wide a band of the silhouette's
/// anti-aliased fringe is decontaminated. Tuned for clean edges on soft/hairy
/// subjects without eroding light features.
pub const DEFAULT_EDGE_SOFTNESS: f32 = 0.55;

/// Fraction of border pixels that must match for a solid matte.
const SOLID_COVERAGE: f32 = 0.90;
/// Fraction of border pixels that must match the reconstructed checker.
const CHECKER_COVERAGE: f32 = 0.95;
/// Minimum alpha for a pixel to count as matte. A "fake transparency" matte is
/// opaque; a pixel that already carries real transparency is left untouched
/// (the lossless `over` identity only holds for opaque source pixels).
const MATTE_MIN_ALPHA: f32 = 1.0 - 1e-4;

/// Pixels of defringe band per unit of [`MatteOptions::edge_softness`]. The band
/// is walked inward from the cleared background into the subject to strip the
/// opaque anti-aliased halo a light matte leaves on the silhouette; its width is
/// `1 + round(edge_softness * this)`, so the default 0.55 gives a 2 px band.
///
/// ponytail: the ramp test below is the real bound (the walk stops at solid
/// subject); this radius only caps how far a *light* subject region touching the
/// silhouette can be softened. Bump it if a very soft, high-res edge needs more.
const DEFRINGE_SOFTNESS_SCALE: f32 = 2.0;

/// Max area, in pixels, of a stray opaque island the despeckle pass will clear.
/// Bits of matte trapped in concavities (between hair strands, fingers, toes)
/// survive as tiny disconnected specks; observed ones are well under this. The
/// subject is one large connected component, so no real feature is ever this
/// small-and-isolated.
///
/// ponytail: fixed 64 px (≈8×8). Scale with resolution if a high-res render
/// leaves larger trapped pockets.
const SPECK_MAX_AREA: usize = 64;

/// Multiple of `tolerance` within which every pixel of a small island must lie
/// for it to count as stray matte (vs a small light subject detail, which is
/// kept). Slightly above `tolerance` so the darker-grey trapped cells that fall
/// just outside the flood are still recognised as background.
const SPECK_TOLERANCE_MULT: f32 = 1.6;

/// Squared-color-distance margin by which a neighbour must be *more subject*
/// (farther from the matte key) than a pixel for that pixel to count as a
/// transitional fringe. A hard solid-subject edge has same-color neighbours
/// (difference ≈ 0), so it stays below this margin and is never unmixed; only
/// a genuine bg→subject anti-aliasing ramp clears it. Sized well above 8-bit
/// compression noise so a flat-but-noisy subject edge is not mistaken for a ramp.
const DEFRINGE_RAMP_MARGIN: f32 = 0.05;

/// Options controlling background-matte removal.
#[derive(Debug, Clone, Copy)]
pub struct MatteOptions {
    /// Color distance within which a pixel is treated as pure background and
    /// cleared to full transparency.
    pub tolerance: f32,
    /// How wide a band of the silhouette's anti-aliased fringe is decontaminated
    /// (given partial alpha + matte-spill removed) to kill the light halo a
    /// cut-out otherwise leaves. This only touches *transitional* edge pixels —
    /// never solid subject — so the background flood itself stays strict and
    /// cannot eat light subject interiors. Larger values clean wider soft edges
    /// but can soften a light subject's boundary; `0` does the minimum 1 px.
    pub edge_softness: f32,
    /// Also remove background-colored regions not connected to the border (e.g.
    /// gaps enclosed by the subject, such as between strands of hair). Catches
    /// trapped background at the cost of also removing matte-colored pixels
    /// inside the subject.
    pub remove_enclosed: bool,
}

impl Default for MatteOptions {
    fn default() -> Self {
        Self {
            tolerance: DEFAULT_MATTE_TOLERANCE,
            edge_softness: DEFAULT_EDGE_SOFTNESS,
            remove_enclosed: false,
        }
    }
}

/// A detected background matte, expressed in **layer-local** coordinates.
#[derive(Debug, Clone, Copy)]
enum MatteKey {
    /// Uniform background fill.
    Solid(Rgba32F),
    /// Transparency checkerboard: two colors on a regular `tile`-sized grid.
    /// `c_even` is the color of cells with even `(cell_x + cell_y)` parity
    /// relative to `origin`.
    Checker {
        c_even: Rgba32F,
        c_odd: Rgba32F,
        tile: i32,
        origin: IVec2,
    },
}

/// Remove the detected background matte from `buffer` with default options.
///
/// Convenience wrapper over [`remove_matte_with`].
pub fn remove_matte(buffer: &TiledBuffer, tolerance: f32) -> Option<TiledBuffer> {
    remove_matte_with(
        buffer,
        MatteOptions {
            tolerance,
            ..MatteOptions::default()
        },
    )
}

/// Remove the detected background matte from `buffer`, returning a new buffer.
///
/// The connected background, flood-filled from the layer's occupied border, is
/// cleared: pixels within `options.tolerance` of the matte color become fully
/// transparent. The flood is **strict** — it only steps through near-matte
/// pixels — so it cannot leak from a background pocket into a light-but-subject
/// region (a white apron, light paint, pale highlights) and erase it. The
/// silhouette's anti-aliased fringe is then cleaned by a separate defringe pass
/// (width set by `edge_softness`): transitional edge pixels are **unmixed** —
/// given partial alpha and their matte-spill decontaminated — which removes the
/// light halo a hard cut leaves behind, without touching solid subject. With
/// `remove_enclosed`, background trapped inside the subject (e.g. gaps between
/// hairs) is cleared too. Subject pixels are left byte-identical except the thin
/// decontaminated fringe. Returns `None` when no solid or checkerboard matte is
/// detected.
pub fn remove_matte_with(buffer: &TiledBuffer, options: MatteOptions) -> Option<TiledBuffer> {
    let inner = options.tolerance.max(0.0);
    let (bounds, data, w, h) = read_bounds(buffer)?;
    let key = detect(&data, w, h, bounds, inner)?;

    // Flood-fill the connected background from the border ring, stepping only
    // through pixels within `inner` of the matte. Anti-aliased edge pixels (a
    // blend of subject + matte) are deliberately *not* swallowed here — widening
    // the flood to reach them is what lets it spill into light subject regions
    // and gut them. The defringe pass below cleans those edges instead.
    let mut region = vec![false; w * h];
    let mut stack = Vec::new();
    let seed = |i: usize, j: usize, region: &mut [bool], stack: &mut Vec<(usize, usize)>| {
        let idx = j * w + i;
        if !region[idx] && is_matte(data[idx], key_at(&key, local_of(bounds, i, j)), inner) {
            region[idx] = true;
            stack.push((i, j));
        }
    };
    for i in 0..w {
        seed(i, 0, &mut region, &mut stack);
        seed(i, h - 1, &mut region, &mut stack);
    }
    for j in 0..h {
        seed(0, j, &mut region, &mut stack);
        seed(w - 1, j, &mut region, &mut stack);
    }

    // Optionally also seed from background trapped inside the subject: any
    // within-`inner` match, regardless of connectivity, so the fringe around an
    // enclosed gap is cleaned just like a border-connected edge.
    if options.remove_enclosed {
        for j in 0..h {
            for i in 0..w {
                if is_matte(data[j * w + i], key_at(&key, local_of(bounds, i, j)), inner) {
                    seed(i, j, &mut region, &mut stack);
                }
            }
        }
    }

    while let Some((i, j)) = stack.pop() {
        let visit = |ni: usize, nj: usize, region: &mut [bool], stack: &mut Vec<(usize, usize)>| {
            let idx = nj * w + ni;
            if !region[idx] && is_matte(data[idx], key_at(&key, local_of(bounds, ni, nj)), inner) {
                region[idx] = true;
                stack.push((ni, nj));
            }
        };
        if i > 0 {
            visit(i - 1, j, &mut region, &mut stack);
        }
        if i + 1 < w {
            visit(i + 1, j, &mut region, &mut stack);
        }
        if j > 0 {
            visit(i, j - 1, &mut region, &mut stack);
        }
        if j + 1 < h {
            visit(i, j + 1, &mut region, &mut stack);
        }
    }

    // Despeckle first: clear small bits of matte trapped in concavities (between
    // hair strands, at the feet, in fabric folds) that the border flood couldn't
    // reach. The defringe below then treats those cleared pockets as part of the
    // cleared set, so the light halo *around* them (e.g. between hair wisps) is
    // decontaminated too — not just the outer silhouette.
    let clear_speck = despeckle_trapped_matte(&region, &data, &key, bounds, w, h, inner);
    let cleared = |idx: usize| region[idx] || clear_speck[idx];

    // Defringe. A sharp silhouette against a near-white (or otherwise light)
    // matte leaves its anti-aliased edge pixels a blend of subject + matte that
    // is *more subject than matte* — beyond the strict flood band above, so they
    // stay fully opaque: a light halo ringing the cut-out. Walk a few pixels
    // inward from the cleared set into those still-contaminated opaque pixels and
    // unmix them too. Only a *transitional* pixel — one with a neighbour that is
    // more subject (farther from the matte key) than itself — is treated as
    // fringe; a hard solid-subject edge has same-colour neighbours and is left
    // byte-identical. That, plus the bounded radius, keeps a saturated body edge
    // un-eroded and never reaches interior light regions walled off by solid
    // subject (e.g. a white apron).
    let defringe_radius =
        1 + (options.edge_softness.max(0.0) * DEFRINGE_SOFTNESS_SCALE).round() as usize;
    let mut defringe = vec![false; w * h];
    let mut frontier: Vec<(usize, usize)> = Vec::new();
    let dist_to_key = |i: usize, j: usize| {
        let idx = j * w + i;
        dist_sq_rgb(sanitize(data[idx]), key_at(&key, local_of(bounds, i, j)))
    };
    let consider =
        |i: usize, j: usize, defringe: &mut [bool], frontier: &mut Vec<(usize, usize)>| {
            let idx = j * w + i;
            if cleared(idx) || defringe[idx] || sanitize(data[idx]).a < MATTE_MIN_ALPHA {
                return;
            }
            // Transitional iff some opaque, non-cleared neighbour is meaningfully
            // more subject (farther from the key) than this pixel.
            let p_dist = dist_to_key(i, j);
            let ramps = |ni: usize, nj: usize| {
                let nidx = nj * w + ni;
                !cleared(nidx)
                    && sanitize(data[nidx]).a >= MATTE_MIN_ALPHA
                    && dist_to_key(ni, nj) > p_dist + DEFRINGE_RAMP_MARGIN
            };
            let is_ramp = (i > 0 && ramps(i - 1, j))
                || (i + 1 < w && ramps(i + 1, j))
                || (j > 0 && ramps(i, j - 1))
                || (j + 1 < h && ramps(i, j + 1));
            if !is_ramp {
                return; // solid subject (or a peak) — a barrier, not fringe
            }
            defringe[idx] = true;
            frontier.push((i, j));
        };
    // Seed from contaminated opaque pixels touching the cleared set…
    for j in 0..h {
        for i in 0..w {
            let idx = j * w + i;
            if cleared(idx) {
                continue;
            }
            let touches = (i > 0 && cleared(idx - 1))
                || (i + 1 < w && cleared(idx + 1))
                || (j > 0 && cleared(idx - w))
                || (j + 1 < h && cleared(idx + w));
            if touches {
                consider(i, j, &mut defringe, &mut frontier);
            }
        }
    }
    // …then grow the band inward up to `defringe_radius` pixels.
    for _ in 1..defringe_radius {
        let mut next = Vec::new();
        for (i, j) in frontier.drain(..) {
            if i > 0 {
                consider(i - 1, j, &mut defringe, &mut next);
            }
            if i + 1 < w {
                consider(i + 1, j, &mut defringe, &mut next);
            }
            if j > 0 {
                consider(i, j - 1, &mut defringe, &mut next);
            }
            if j + 1 < h {
                consider(i, j + 1, &mut defringe, &mut next);
            }
        }
        frontier = next;
    }

    // Write the result. Background *interior* to the region clears to full
    // transparency regardless of distance — so a non-flat matte (a shadow or
    // gradient on the backdrop) does not leave dark partial-alpha residue. Only
    // the region's *boundary ring* against the subject, and only where it is
    // beyond the inner tolerance (a genuine anti-aliased blend), is unmixed into
    // a soft, decontaminated edge — this removes the white fringe.
    //
    // A neighbour outside `bounds` counts as background (the matte runs to the
    // image edge), so image-edge pixels stay interior, not boundary.
    //
    // ponytail: reads the whole occupied bounds into one Vec and clones the
    // buffer — peak ~2× the layer in RAM. Fine for a deliberate one-shot edit;
    // stream tile-by-tile if max-canvas memory ever bites.
    // A region pixel is on the subject boundary only if a neighbour is NOT
    // cleared (region ∪ despeckled matte) — a cleared neighbour is still
    // background, not subject.
    let is_boundary = |i: usize, j: usize| {
        (i > 0 && !cleared(j * w + i - 1))
            || (i + 1 < w && !cleared(j * w + i + 1))
            || (j > 0 && !cleared((j - 1) * w + i))
            || (j + 1 < h && !cleared((j + 1) * w + i))
    };
    let inner_sq = inner * inner;
    let mut out = buffer.clone();
    for j in 0..h {
        for i in 0..w {
            let idx = j * w + i;
            let local = local_of(bounds, i, j);
            if cleared(idx) {
                // Cleared background (flood region or despeckled trapped matte).
                // Interior clears outright; a boundary pixel against the subject
                // that is beyond the inner tolerance (a genuine anti-aliased
                // blend) is unmixed into a soft, decontaminated edge instead of
                // zeroed — this is what removes the white fringe, including the
                // halo between hair strands around a despeckled pocket.
                let src = data[idx];
                let k = key_at(&key, local);
                let px = if is_boundary(i, j) && dist_sq_rgb(sanitize(src), k) > inner_sq {
                    unmix(src, k)
                } else {
                    Rgba32F::TRANSPARENT
                };
                out.set_pixel(local, px);
            } else if defringe[idx] {
                // Contaminated anti-aliased edge pixel just outside the cleared
                // set: unmix away the matte spill, recovering its partial alpha.
                out.set_pixel(local, unmix(data[idx], key_at(&key, local)));
            }
        }
    }
    out.prune_empty_tiles();
    Some(out)
}

/// Clear small bits of matte trapped in concavities the border flood couldn't
/// reach, returning a `w*h` mask of pixels to make transparent.
///
/// 8-connected components are grown over the *matte-coloured* opaque pixels the
/// flood left behind, walking only through matte-coloured pixels — so the
/// subject's own (non-matte) body walls off each trapped pocket as its own
/// component. A component is cleared when it is both small (≤ [`SPECK_MAX_AREA`])
/// and touches the flood `region` — stray trapped matte at the silhouette. A
/// small component buried in the interior (an eye catchlight, teeth) touches no
/// exterior and is kept; a large one (a white paint dab) is kept by size;
/// non-matte detail is never a candidate.
fn despeckle_trapped_matte(
    region: &[bool],
    data: &[Rgba32F],
    key: &MatteKey,
    bounds: Rect,
    w: usize,
    h: usize,
    inner: f32,
) -> Vec<bool> {
    let speck_tol_sq = (inner * SPECK_TOLERANCE_MULT).powi(2);
    let candidate = |idx: usize, i: usize, j: usize| {
        !region[idx]
            && sanitize(data[idx]).a >= MATTE_MIN_ALPHA
            && dist_sq_rgb(sanitize(data[idx]), key_at(key, local_of(bounds, i, j))) <= speck_tol_sq
    };
    let mut visited = vec![false; w * h];
    let mut clear_speck = vec![false; w * h];
    let mut comp: Vec<usize> = Vec::new();
    let mut bfs: Vec<(usize, usize)> = Vec::new();
    for sj in 0..h {
        for si in 0..w {
            let s = sj * w + si;
            if visited[s] || !candidate(s, si, sj) {
                continue;
            }
            comp.clear();
            bfs.clear();
            bfs.push((si, sj));
            visited[s] = true;
            let mut touches_region = false;
            let mut too_big = false;
            while let Some((i, j)) = bfs.pop() {
                let idx = j * w + i;
                if comp.len() > SPECK_MAX_AREA {
                    too_big = true; // a real feature, not a speck — stop tracking
                } else {
                    comp.push(idx);
                }
                let x0 = i.saturating_sub(1);
                let x1 = (i + 1).min(w - 1);
                let y0 = j.saturating_sub(1);
                let y1 = (j + 1).min(h - 1);
                for nj in y0..=y1 {
                    for ni in x0..=x1 {
                        let nidx = nj * w + ni;
                        if region[nidx] {
                            touches_region = true;
                        } else if !visited[nidx] && candidate(nidx, ni, nj) {
                            visited[nidx] = true;
                            bfs.push((ni, nj));
                        }
                    }
                }
            }
            if !too_big && touches_region {
                for &idx in &comp {
                    clear_speck[idx] = true;
                }
            }
        }
    }
    clear_speck
}

/// Layer-local coordinate of border-relative cell `(i, j)`.
fn local_of(bounds: Rect, i: usize, j: usize) -> IVec2 {
    IVec2::new(bounds.x + i as i32, bounds.y + j as i32)
}

/// Read a buffer's exact occupied bounds into a row-major `Vec`.
///
/// Returns `None` for an empty buffer or one whose occupied bounds (which can
/// span far-apart sparse pixels) are too large to materialize — guarding the
/// `read_rect` overflow panic.
fn read_bounds(buffer: &TiledBuffer) -> Option<(Rect, Vec<Rgba32F>, usize, usize)> {
    let bounds = buffer.exact_bounds()?;
    let w = bounds.w as usize;
    let h = bounds.h as usize;
    if w == 0 || h == 0 {
        return None;
    }
    // `read_rect` panics if `w * h` overflows `u32`; a degenerate sparse layer
    // has no detectable matte anyway, so bail instead.
    if (w as u64) * (h as u64) > u32::MAX as u64 {
        return None;
    }
    let data = buffer.read_rect(bounds);
    Some((bounds, data, w, h))
}

fn sanitize(px: Rgba32F) -> Rgba32F {
    let fix = |v: f32| if v.is_nan() { 0.0 } else { v };
    Rgba32F::new(fix(px.r), fix(px.g), fix(px.b), fix(px.a))
}

fn dist_sq_rgb(a: Rgba32F, b: Rgba32F) -> f32 {
    let dr = a.r - b.r;
    let dg = a.g - b.g;
    let db = a.b - b.b;
    dr * dr + dg * dg + db * db
}

fn matches(px: Rgba32F, key: Rgba32F, tolerance: f32) -> bool {
    dist_sq_rgb(sanitize(px), key) <= tolerance * tolerance
}

/// Whether `px` is a matte pixel: opaque and within `tolerance` of `key`.
///
/// The opacity gate keeps the flood region from swallowing pixels that already
/// carry real transparency, so they are preserved exactly.
fn is_matte(px: Rgba32F, key: Rgba32F, tolerance: f32) -> bool {
    sanitize(px).a >= MATTE_MIN_ALPHA && matches(px, key, tolerance)
}

/// The background color the matte predicts at a given layer-local pixel.
fn key_at(key: &MatteKey, local: IVec2) -> Rgba32F {
    match key {
        MatteKey::Solid(c) => *c,
        MatteKey::Checker {
            c_even,
            c_odd,
            tile,
            origin,
        } => {
            let jx = (local.x - origin.x).div_euclid(*tile);
            let jy = (local.y - origin.y).div_euclid(*tile);
            if (jx + jy).rem_euclid(2) == 0 {
                *c_even
            } else {
                *c_odd
            }
        }
    }
}

/// Color-to-alpha unmix of pixel `p` against matte color `k`.
///
/// Returns the straight-alpha pixel that, composited over `k`, reproduces `p`
/// exactly (for an opaque `p`; the flood region only ever feeds opaque pixels).
fn unmix(p: Rgba32F, k: Rgba32F) -> Rgba32F {
    let p = sanitize(p);
    let a = coverage(p.r, k.r)
        .max(coverage(p.g, k.g))
        .max(coverage(p.b, k.b))
        .clamp(0.0, 1.0);
    if a <= 0.0 {
        // Zero coverage means `p` equals the matte (→ transparent) or lies
        // outside what the matte can explain — e.g. an HDR highlight brighter
        // than a white matte, where `coverage`'s clamp guard returns 0. The
        // latter is not background, so keep it rather than discarding its
        // energy.
        return if dist_sq_rgb(p, k) <= f32::EPSILON {
            Rgba32F::TRANSPARENT
        } else {
            p
        };
    }
    let recover = |pc: f32, kc: f32| (pc - kc) / a + kc;
    Rgba32F::new(
        recover(p.r, k.r),
        recover(p.g, k.g),
        recover(p.b, k.b),
        p.a * a,
    )
}

/// Per-channel coverage required to explain `pc` as `fg` composited over `kc`.
///
/// Guards against divide-by-zero so HDR overshoot (`pc > 1`) never yields `inf`.
fn coverage(pc: f32, kc: f32) -> f32 {
    if pc < kc && kc > 0.0 {
        (kc - pc) / kc
    } else if pc > kc && kc < 1.0 {
        (pc - kc) / (1.0 - kc)
    } else {
        0.0
    }
}

/// Detect a matte from the buffer border: solid first, then checkerboard.
fn detect(data: &[Rgba32F], w: usize, h: usize, bounds: Rect, tolerance: f32) -> Option<MatteKey> {
    let border = border_pixels(data, w, h, bounds);
    detect_solid(&border, tolerance)
        .or_else(|| detect_checker(data, w, h, bounds, &border, tolerance))
}

/// Collect the border ring as `(layer-local coord, color)` pairs.
fn border_pixels(data: &[Rgba32F], w: usize, h: usize, bounds: Rect) -> Vec<(IVec2, Rgba32F)> {
    let mut out = Vec::with_capacity(2 * (w + h));
    let push = |i: usize, j: usize, out: &mut Vec<(IVec2, Rgba32F)>| {
        out.push((local_of(bounds, i, j), sanitize(data[j * w + i])));
    };
    for i in 0..w {
        push(i, 0, &mut out);
        push(i, h - 1, &mut out);
    }
    for j in 0..h {
        push(0, j, &mut out);
        push(w - 1, j, &mut out);
    }
    out
}

fn detect_solid(border: &[(IVec2, Rgba32F)], tolerance: f32) -> Option<MatteKey> {
    // Seed the key with the mean of all border pixels, then re-average over only
    // the within-tolerance inliers a few times. `SOLID_COVERAGE` tolerates up to
    // 10% non-matte pixels (e.g. a subject touching the border); without this
    // refinement those outliers skew the key off the true matte color, and the
    // boundary-ring unmix then runs against the wrong color.
    let mut key = mean_color(border.iter().map(|(_, c)| *c));
    for _ in 0..4 {
        let inliers = border
            .iter()
            .map(|(_, c)| *c)
            .filter(|c| dist_sq_rgb(*c, key) <= tolerance * tolerance);
        let refined = mean_color(inliers);
        if dist_sq_rgb(refined, key) <= f32::EPSILON {
            key = refined;
            break;
        }
        key = refined;
    }
    let within = border
        .iter()
        .filter(|(_, c)| dist_sq_rgb(*c, key) <= tolerance * tolerance)
        .count();
    if within as f32 >= border.len() as f32 * SOLID_COVERAGE {
        Some(MatteKey::Solid(key))
    } else {
        None
    }
}

fn detect_checker(
    data: &[Rgba32F],
    w: usize,
    h: usize,
    bounds: Rect,
    border: &[(IVec2, Rgba32F)],
    tolerance: f32,
) -> Option<MatteKey> {
    let (c_a, c_b) = two_means(border.iter().map(|(_, c)| *c))?;
    // The two cluster colors must be distinguishable, else it is effectively a
    // solid (and a single key matches everything).
    if dist_sq_rgb(c_a, c_b) <= tolerance * tolerance {
        return None;
    }

    let classify = |c: Rgba32F| -> usize {
        if dist_sq_rgb(c, c_a) <= dist_sq_rgb(c, c_b) {
            0
        } else {
            1
        }
    };

    // Tile size from the smallest recurring transition gap along the top row
    // and left column (both edges are background for a real matte).
    let top: Vec<usize> = (0..w).map(|i| classify(sanitize(data[i]))).collect();
    let left: Vec<usize> = (0..h).map(|j| classify(sanitize(data[j * w]))).collect();
    let top_tr = transitions(&top);
    let left_tr = transitions(&left);
    let mut gaps = Vec::new();
    gaps.extend(consecutive_gaps(&top_tr));
    gaps.extend(consecutive_gaps(&left_tr));
    let tile = mode(&gaps)? as i32;
    let x0 = *top_tr.first()? as i32 + bounds.x;
    let y0 = *left_tr.first()? as i32 + bounds.y;
    let origin = IVec2::new(x0, y0);

    // Decide which cluster sits on even parity by voting over the border.
    let mut agree = 0i64;
    let mut disagree = 0i64;
    for (local, c) in border {
        let jx = (local.x - x0).div_euclid(tile);
        let jy = (local.y - y0).div_euclid(tile);
        let even = (jx + jy).rem_euclid(2) == 0;
        let cluster0 = classify(*c) == 0;
        if even == cluster0 {
            agree += 1;
        } else {
            disagree += 1;
        }
    }
    let (c_even, c_odd) = if agree >= disagree {
        (c_a, c_b)
    } else {
        (c_b, c_a)
    };

    let key = MatteKey::Checker {
        c_even,
        c_odd,
        tile,
        origin,
    };
    let ok = border
        .iter()
        .filter(|(local, c)| dist_sq_rgb(*c, key_at(&key, *local)) <= tolerance * tolerance)
        .count();
    if ok as f32 >= border.len() as f32 * CHECKER_COVERAGE {
        Some(key)
    } else {
        None
    }
}

fn mean_color(colors: impl Iterator<Item = Rgba32F>) -> Rgba32F {
    let (mut r, mut g, mut b, mut n) = (0.0f64, 0.0f64, 0.0f64, 0u64);
    for c in colors {
        r += c.r as f64;
        g += c.g as f64;
        b += c.b as f64;
        n += 1;
    }
    if n == 0 {
        return Rgba32F::new(0.0, 0.0, 0.0, 1.0);
    }
    let n = n as f64;
    Rgba32F::new((r / n) as f32, (g / n) as f32, (b / n) as f32, 1.0)
}

/// Two-color k-means over a color set, seeded by the luminance extremes.
fn two_means(colors: impl Iterator<Item = Rgba32F>) -> Option<(Rgba32F, Rgba32F)> {
    let pts: Vec<Rgba32F> = colors.collect();
    if pts.len() < 2 {
        return None;
    }
    let luma = |c: &Rgba32F| c.r + c.g + c.b;
    let mut m0 = *pts
        .iter()
        .min_by(|a, b| luma(a).total_cmp(&luma(b)))
        .unwrap();
    let mut m1 = *pts
        .iter()
        .max_by(|a, b| luma(a).total_cmp(&luma(b)))
        .unwrap();
    for _ in 0..8 {
        let mut s0 = (0.0f64, 0.0f64, 0.0f64, 0u64);
        let mut s1 = (0.0f64, 0.0f64, 0.0f64, 0u64);
        for c in &pts {
            let s = if dist_sq_rgb(*c, m0) <= dist_sq_rgb(*c, m1) {
                &mut s0
            } else {
                &mut s1
            };
            s.0 += c.r as f64;
            s.1 += c.g as f64;
            s.2 += c.b as f64;
            s.3 += 1;
        }
        let centroid = |s: (f64, f64, f64, u64), fallback: Rgba32F| {
            if s.3 == 0 {
                fallback
            } else {
                let n = s.3 as f64;
                Rgba32F::new((s.0 / n) as f32, (s.1 / n) as f32, (s.2 / n) as f32, 1.0)
            }
        };
        m0 = centroid(s0, m0);
        m1 = centroid(s1, m1);
    }
    Some((m0, m1))
}

/// Indices `i` where `seq[i] != seq[i - 1]`.
fn transitions(seq: &[usize]) -> Vec<usize> {
    (1..seq.len()).filter(|&i| seq[i] != seq[i - 1]).collect()
}

/// Gaps between consecutive transition indices.
fn consecutive_gaps(tr: &[usize]) -> Vec<usize> {
    tr.windows(2).map(|w| w[1] - w[0]).collect()
}

/// Most frequent value (smallest on ties), or `None` if empty.
fn mode(values: &[usize]) -> Option<usize> {
    let mut counts: std::collections::BTreeMap<usize, usize> = std::collections::BTreeMap::new();
    for &v in values {
        *counts.entry(v).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(v, _)| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn px(r: f32, g: f32, b: f32, a: f32) -> Rgba32F {
        Rgba32F::new(r, g, b, a)
    }

    /// Straight-alpha "over" against an opaque background.
    fn over(fg: Rgba32F, bg: Rgba32F) -> Rgba32F {
        Rgba32F::new(
            fg.r * fg.a + bg.r * (1.0 - fg.a),
            fg.g * fg.a + bg.g * (1.0 - fg.a),
            fg.b * fg.a + bg.b * (1.0 - fg.a),
            1.0,
        )
    }

    fn close(a: Rgba32F, b: Rgba32F, eps: f32) -> bool {
        (a.r - b.r).abs() <= eps
            && (a.g - b.g).abs() <= eps
            && (a.b - b.b).abs() <= eps
            && (a.a - b.a).abs() <= eps
    }

    // ------------------------------------------------------------------
    // Lossless round-trip — the core guarantee.
    // ------------------------------------------------------------------

    proptest! {
        #[test]
        fn unmix_recomposite_reproduces_original(
            k in prop::array::uniform3(0.0f32..=1.0),
            f in prop::array::uniform3(0.0f32..=1.0),
            cov in 0.0f32..=1.0,
        ) {
            let key = px(k[0], k[1], k[2], 1.0);
            let fg = px(f[0], f[1], f[2], cov);
            let original = over(fg, key);            // flatten subject over matte
            let lifted = unmix(original, key);       // lift back to alpha
            let recomposited = over(lifted, key);    // re-flatten over the same matte
            prop_assert!(close(original, recomposited, 1e-4),
                "orig {:?} != recomposite {:?}", original, recomposited);
        }
    }

    #[test]
    fn unmix_exact_key_is_transparent() {
        assert_eq!(
            unmix(px(1.0, 1.0, 1.0, 1.0), px(1.0, 1.0, 1.0, 1.0)),
            Rgba32F::TRANSPARENT
        );
        assert_eq!(
            unmix(px(0.5, 0.5, 0.5, 1.0), px(0.5, 0.5, 0.5, 1.0)),
            Rgba32F::TRANSPARENT
        );
    }

    #[test]
    fn unmix_opaque_subject_stays_opaque() {
        // Pure black over white is fully opaque black.
        let out = unmix(px(0.0, 0.0, 0.0, 1.0), px(1.0, 1.0, 1.0, 1.0));
        assert!(close(out, px(0.0, 0.0, 0.0, 1.0), 1e-6));
    }

    #[test]
    fn unmix_half_edge_recovers_half_alpha() {
        // Red at 50% over white = (1.0, 0.5, 0.5). Unmix vs white -> ~50% red.
        let edge = over(px(1.0, 0.0, 0.0, 0.5), px(1.0, 1.0, 1.0, 1.0));
        let out = unmix(edge, px(1.0, 1.0, 1.0, 1.0));
        assert!((out.a - 0.5).abs() <= 1e-4, "alpha {}", out.a);
        assert!(close(over(out, px(1.0, 1.0, 1.0, 1.0)), edge, 1e-4));
    }

    #[test]
    fn unmix_guards_hdr_overshoot() {
        let out = unmix(px(1.5, 0.2, 0.2, 1.0), px(1.0, 1.0, 1.0, 1.0));
        assert!(out.r.is_finite() && out.g.is_finite() && out.b.is_finite() && out.a.is_finite());
    }

    #[test]
    fn unmix_hdr_over_clamped_matte_is_preserved_not_zeroed() {
        // A highlight brighter than a white matte cannot be background; it must
        // be kept, not discarded to transparent.
        let white = px(1.0, 1.0, 1.0, 1.0);
        assert!(close(
            unmix(px(1.5, 1.0, 1.0, 1.0), white),
            px(1.5, 1.0, 1.0, 1.0),
            1e-6
        ));
        // Symmetric case against a black matte at the lower clamp.
        let black = px(0.0, 0.0, 0.0, 1.0);
        assert!(close(
            unmix(px(-0.3, 0.0, 0.0, 1.0), black),
            px(-0.3, 0.0, 0.0, 1.0),
            1e-6
        ));
    }

    // ------------------------------------------------------------------
    // Solid matte removal.
    // ------------------------------------------------------------------

    fn filled(w: i32, h: i32, color: Rgba32F) -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..h {
            for x in 0..w {
                buf.set_pixel(IVec2::new(x, y), color);
            }
        }
        buf
    }

    #[test]
    fn solid_white_interior_becomes_transparent_subject_kept() {
        let mut buf = filled(40, 40, px(1.0, 1.0, 1.0, 1.0));
        // Opaque red subject square in the middle.
        for y in 15..25 {
            for x in 15..25 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 0.0, 0.0, 1.0));
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        assert_eq!(out.get_pixel(IVec2::new(2, 2)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(20, 20)), px(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn non_flat_white_background_clears_to_transparent() {
        // Regression: a near-white background that is NOT perfectly flat (mild
        // gradient / compression noise) must still clear fully. Keying on the
        // mean and unmixing every pixel left brighter-than-mean background with
        // large residual alpha; interior pixels now go fully transparent.
        let mut buf = TiledBuffer::new();
        for y in 0..40 {
            for x in 0..40 {
                // Background ramps 0.85..=1.0 across the canvas — all within
                // tolerance of the mean, so it is one connected region.
                let v = 0.85 + 0.15 * (x as f32 / 39.0);
                buf.set_pixel(IVec2::new(x, y), px(v, v, v, 1.0));
            }
        }
        // Distinct opaque subject in the middle.
        for y in 16..24 {
            for x in 16..24 {
                buf.set_pixel(IVec2::new(x, y), px(0.1, 0.2, 0.9, 1.0));
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        // Interior background everywhere clears, regardless of local brightness.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(39, 0)), Rgba32F::TRANSPARENT); // brightest corner
        assert_eq!(out.get_pixel(IVec2::new(5, 20)), Rgba32F::TRANSPARENT);
        // Subject untouched.
        assert_eq!(out.get_pixel(IVec2::new(20, 20)), px(0.1, 0.2, 0.9, 1.0));
    }

    #[test]
    fn subject_touching_border_does_not_skew_removal() {
        // A subject on the border is part of the <=10% that detection tolerates;
        // the key must still resolve to the true matte color so the background
        // (including the column hugging the subject) clears completely.
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(40, 40, white);
        for y in 14..26 {
            for x in 34..40 {
                buf.set_pixel(IVec2::new(x, y), px(0.0, 0.0, 0.0, 1.0));
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(33, 20)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(37, 20)), px(0.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn semi_transparent_pixel_near_matte_is_preserved() {
        // A pixel that already carries real transparency is not a fake matte;
        // it must be left byte-identical even when its RGB is near the matte.
        let mut buf = filled(40, 40, px(1.0, 1.0, 1.0, 1.0));
        let ghost = px(0.99, 0.99, 0.99, 0.5);
        buf.set_pixel(IVec2::new(5, 5), ghost);
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        assert_eq!(out.get_pixel(IVec2::new(5, 5)), ghost);
        assert_eq!(out.get_pixel(IVec2::new(20, 20)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn solid_offwhite_253_is_detected() {
        let off = 253.0 / 255.0;
        let buf = filled(20, 20, px(off, off, off, 1.0));
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        assert_eq!(out.get_pixel(IVec2::new(5, 5)), Rgba32F::TRANSPARENT);
    }

    #[test]
    fn enclosed_background_color_is_not_holed() {
        // A red ring around a white-colored hole that is NOT connected to the
        // border: the hole must stay opaque white (connected-from-border only).
        let mut buf = filled(40, 40, px(1.0, 1.0, 1.0, 1.0));
        for y in 10..30 {
            for x in 10..30 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 0.0, 0.0, 1.0)); // solid red block
            }
        }
        // Punch a white hole in the middle of the red block, enclosed by red.
        for y in 18..22 {
            for x in 18..22 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 1.0, 1.0, 1.0));
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        assert_eq!(out.get_pixel(IVec2::new(1, 1)), Rgba32F::TRANSPARENT); // outer bg gone
        assert_eq!(out.get_pixel(IVec2::new(20, 20)), px(1.0, 1.0, 1.0, 1.0)); // hole kept
        assert_eq!(out.get_pixel(IVec2::new(12, 12)), px(1.0, 0.0, 0.0, 1.0)); // red kept
    }

    #[test]
    fn despeckle_clears_stranded_matte_specks() {
        // Bits of checkerboard trapped in concavities (between hair strands,
        // fingers, toes) sit just past tolerance AND are cut off from the border
        // flood, so they survive as tiny light specks dotting the cut-out. A
        // small, matte-coloured, *disconnected* island must be cleared — while
        // connected subject and small *non-matte* flecks are kept.
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(48, 48, white);
        let subject = px(0.1, 0.4, 0.1, 1.0);
        for y in 10..38 {
            for x in 10..38 {
                buf.set_pixel(IVec2::new(x, y), subject);
            }
        }
        // Stray light-grey speck in the background: ~0.26 from white, so past the
        // 0.2 tolerance (flood leaves it) but within the despeckle band.
        let speck = px(0.85, 0.85, 0.85, 1.0);
        for y in 3..6 {
            for x in 3..6 {
                buf.set_pixel(IVec2::new(x, y), speck);
            }
        }
        // A small saturated fleck stranded in the background must be KEPT.
        let fleck = px(0.9, 0.1, 0.1, 1.0);
        buf.set_pixel(IVec2::new(44, 3), fleck);
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        assert_eq!(out.get_pixel(IVec2::new(4, 4)), Rgba32F::TRANSPARENT); // speck cleared
        assert_eq!(out.get_pixel(IVec2::new(24, 24)), subject); // subject kept
        assert_eq!(out.get_pixel(IVec2::new(44, 3)), fleck); // non-matte fleck kept
    }

    #[test]
    fn antialiased_edge_recovers_partial_alpha_losslessly() {
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(40, 40, white);
        // Solid red block with a 50% red/white anti-aliased edge column. The
        // edge connects the white background to the block.
        let edge_orig = over(px(1.0, 0.0, 0.0, 0.5), white);
        for y in 10..30 {
            for x in 20..30 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 0.0, 0.0, 1.0));
            }
            buf.set_pixel(IVec2::new(19, y), edge_orig);
        }
        // Inner tolerance below the 50% edge (dist ~0.71 from white) so it falls
        // in the unmix band (inner, inner + edge_softness], not cleared outright;
        // the solid block (dist ~1.41) lies beyond the band and is untouched.
        let out = remove_matte(&buf, 0.4).expect("matte detected");

        let edge = out.get_pixel(IVec2::new(19, 20));
        assert!((edge.a - 0.5).abs() <= 1e-3, "edge alpha {}", edge.a);
        // Lossless: recompositing the recovered edge over white reproduces it.
        assert!(close(over(edge, white), edge_orig, 1e-4));
        // The opaque block is left untouched.
        assert_eq!(out.get_pixel(IVec2::new(25, 20)), px(1.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn defringe_cleans_a_mostly_white_edge() {
        // A subject block with a mostly-white fringe ring (80% white) beyond the
        // inner tolerance. The old hard-threshold left it opaque (white halo);
        // the edge band now unmixes it to a low alpha so the halo disappears.
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(40, 40, white);
        let fringe = over(px(1.0, 0.0, 0.0, 0.2), white); // ~0.28 from white
        for y in 10..30 {
            for x in 20..30 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 0.0, 0.0, 1.0));
            }
            buf.set_pixel(IVec2::new(19, y), fringe);
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        let edge = out.get_pixel(IVec2::new(19, 20));
        assert!(
            edge.a < 0.35,
            "fringe should be mostly cleared, alpha {}",
            edge.a
        );
        // Re-compositing the cleaned fringe over white still reproduces it.
        assert!(close(over(edge, white), fringe, 1e-4));
    }

    #[test]
    fn defringe_clears_opaque_aa_halo_beyond_band() {
        // The real-world halo: a sharp silhouette against a near-white matte
        // leaves a single anti-aliased pixel that is *more subject than matte*,
        // so it sits just BEYOND the outer flood band and the old code kept it
        // fully opaque — a light rim around the cut-out. It must now be unmixed
        // to partial alpha and decontaminated, losslessly, while solid subject
        // one pixel further in is left untouched (not eroded).
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(40, 40, white);
        let subject = px(0.05, 0.25, 0.02, 1.0); // dark, saturated -> a barrier
        let aa = over(px(subject.r, subject.g, subject.b, 0.55), white); // ~0.87 from white
        for y in 5..35 {
            for x in 20..35 {
                buf.set_pixel(IVec2::new(x, y), subject);
            }
            buf.set_pixel(IVec2::new(19, y), aa);
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        let edge = out.get_pixel(IVec2::new(19, 20));
        assert!(
            edge.a < 0.95,
            "halo pixel must be unmixed, alpha {}",
            edge.a
        );
        // Lossless: recompositing the recovered edge over white reproduces it.
        assert!(
            close(over(edge, white), aa, 1e-3),
            "not lossless: {:?}",
            over(edge, white)
        );
        // Solid subject just inside the halo is a barrier — left byte-identical.
        assert_eq!(out.get_pixel(IVec2::new(20, 20)), subject);
        assert_eq!(out.get_pixel(IVec2::new(27, 20)), subject);
    }

    #[test]
    fn light_subject_connected_to_background_is_not_eaten() {
        // The real-world failure that motivated the strict flood: a near-white
        // matte with a large LIGHT subject region (a cream apron, light paint,
        // pale highlights). Such pixels are beyond `tolerance` but close enough
        // that an over-wide flood band leaks straight through them and guts the
        // subject. The strict flood must clear the surrounding matte while leaving
        // every light-subject pixel — interior AND hard edge — byte-identical.
        let white = px(1.0, 1.0, 1.0, 1.0);
        let mut buf = filled(48, 48, white);
        // Light-grey block ~0.69 from white: well beyond the 0.2 tolerance, but
        // well within the 0.75 band the old flood used — which devoured it.
        let light = px(0.6, 0.6, 0.6, 1.0);
        for y in 12..36 {
            for x in 12..36 {
                buf.set_pixel(IVec2::new(x, y), light);
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("matte detected");
        // Surrounding matte cleared…
        assert_eq!(out.get_pixel(IVec2::new(2, 2)), Rgba32F::TRANSPARENT);
        // …but the light subject is fully preserved, including its hard edge
        // (a hard edge has same-colour neighbours, so defringe leaves it alone).
        assert_eq!(out.get_pixel(IVec2::new(24, 24)), light); // interior
        assert_eq!(out.get_pixel(IVec2::new(12, 24)), light); // left edge
        assert_eq!(out.get_pixel(IVec2::new(35, 24)), light); // right edge
    }

    #[test]
    fn remove_enclosed_clears_trapped_background() {
        // A red block with a white hole fully enclosed by red.
        let mut buf = filled(40, 40, px(1.0, 1.0, 1.0, 1.0));
        for y in 10..30 {
            for x in 10..30 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 0.0, 0.0, 1.0));
            }
        }
        for y in 18..22 {
            for x in 18..22 {
                buf.set_pixel(IVec2::new(x, y), px(1.0, 1.0, 1.0, 1.0));
            }
        }
        let out = remove_matte_with(
            &buf,
            MatteOptions {
                remove_enclosed: true,
                ..MatteOptions::default()
            },
        )
        .expect("matte detected");
        // With remove_enclosed, the trapped white hole clears too.
        assert_eq!(out.get_pixel(IVec2::new(20, 20)), Rgba32F::TRANSPARENT);
        // The red subject is still kept.
        assert_eq!(out.get_pixel(IVec2::new(12, 12)), px(1.0, 0.0, 0.0, 1.0));
    }

    // ------------------------------------------------------------------
    // Checkerboard matte removal.
    // ------------------------------------------------------------------

    fn checker(w: i32, h: i32, tile: i32, c0: Rgba32F, c1: Rgba32F) -> TiledBuffer {
        let mut buf = TiledBuffer::new();
        for y in 0..h {
            for x in 0..w {
                let parity = ((x / tile) + (y / tile)).rem_euclid(2);
                buf.set_pixel(IVec2::new(x, y), if parity == 0 { c0 } else { c1 });
            }
        }
        buf
    }

    fn assert_checker_removed(c0: Rgba32F, c1: Rgba32F) {
        let mut buf = checker(48, 48, 8, c0, c1);
        // Opaque blue subject in the middle, distinct from both checker colors.
        for y in 20..28 {
            for x in 20..28 {
                buf.set_pixel(IVec2::new(x, y), px(0.0, 0.0, 1.0, 1.0));
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).expect("checker detected");
        // Several near-border checker pixels (both parities) gone.
        assert_eq!(out.get_pixel(IVec2::new(0, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(8, 0)), Rgba32F::TRANSPARENT);
        assert_eq!(out.get_pixel(IVec2::new(0, 8)), Rgba32F::TRANSPARENT);
        // Subject untouched.
        assert_eq!(out.get_pixel(IVec2::new(24, 24)), px(0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn checker_white_grey_removed() {
        assert_checker_removed(px(1.0, 1.0, 1.0, 1.0), px(0.6, 0.6, 0.6, 1.0));
    }

    #[test]
    fn checker_white_black_removed() {
        assert_checker_removed(px(1.0, 1.0, 1.0, 1.0), px(0.0, 0.0, 0.0, 1.0));
    }

    #[test]
    fn checker_grey_darkgrey_removed() {
        assert_checker_removed(px(0.6, 0.6, 0.6, 1.0), px(0.3, 0.3, 0.3, 1.0));
    }

    // ------------------------------------------------------------------
    // Negative / edge cases.
    // ------------------------------------------------------------------

    #[test]
    fn empty_buffer_returns_none() {
        assert!(remove_matte(&TiledBuffer::new(), DEFAULT_MATTE_TOLERANCE).is_none());
    }

    #[test]
    fn degenerate_sparse_bounds_does_not_panic() {
        // Two pixels at opposite extremes give bounds whose area overflows u32.
        let mut buf = TiledBuffer::new();
        buf.set_pixel(IVec2::new(0, 0), px(1.0, 1.0, 1.0, 1.0));
        buf.set_pixel(IVec2::new(1_000_000, 1_000_000), px(1.0, 1.0, 1.0, 1.0));
        assert!(remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).is_none());
    }

    #[test]
    fn gradient_border_has_no_matte() {
        // Each column a distinct value: not uniform, not a clean checker.
        let mut buf = TiledBuffer::new();
        for y in 0..16 {
            for x in 0..16 {
                let v = x as f32 / 15.0;
                buf.set_pixel(IVec2::new(x, y), px(v, 1.0 - v, (v * 0.5).fract(), 1.0));
            }
        }
        assert!(remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).is_none());
    }

    #[test]
    fn non_matte_pixels_are_byte_identical() {
        let mut buf = filled(40, 40, px(1.0, 1.0, 1.0, 1.0));
        let subject = px(0.2, 0.7, 0.3, 1.0);
        for y in 15..25 {
            for x in 15..25 {
                buf.set_pixel(IVec2::new(x, y), subject);
            }
        }
        let out = remove_matte(&buf, DEFAULT_MATTE_TOLERANCE).unwrap();
        // Every subject pixel is preserved exactly.
        for y in 15..25 {
            for x in 15..25 {
                assert_eq!(out.get_pixel(IVec2::new(x, y)), subject);
            }
        }
    }
}
