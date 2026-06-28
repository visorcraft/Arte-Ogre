// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

// Multi-layer batched blend shader for Arte Ogre.
//
// This shader composites an ordered *run* of source tiles over a single
// destination accumulator tile in one dispatch. Each source tile is one slice
// of a `texture_2d_array`; `layers[i]` carries that slice's blend mode,
// opacity, source offset, and mask flag. The accumulator is kept in a register
// across the whole run and written exactly once, replacing the N separate
// read-modify-write dispatches of `blend.wgsl`'s `composite` entry point.
//
// The per-contribution math is identical to `blend.wgsl::composite`; see
// `composite_over` below. The two shaders are validated against each other and
// against the CPU reference in the golden tests (`1e-4` per channel).

struct LayerParam {
    // Blend-mode index, matching the MODE_* constants below.
    mode: u32,
    // Opacity in [0, 1] (NaN already sanitized to 0 on the host).
    opacity: f32,
    // Pixel offset applied to source coordinates: source is read at
    // `dst_coord - offset`.
    offset_x: i32,
    offset_y: i32,
    // 1 when this contribution has a real mask slice in `mask_arr`, 0 otherwise
    // (in which case coverage is a constant 1.0 and `mask_arr` is not sampled).
    has_mask: u32,
    pad0: u32,
    pad1: u32,
    pad2: u32,
}

@group(0) @binding(0) var src_arr: texture_2d_array<f32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba32float, read_write>;
@group(0) @binding(2) var<storage, read> layers: array<LayerParam>;
// Parallel mask array. When a run has no masked contributions the host binds
// `src_arr` here too; it is never sampled in that case (every `has_mask == 0`).
@group(0) @binding(3) var mask_arr: texture_2d_array<f32>;

const MODE_NORMAL: u32 = 0u;
const MODE_MULTIPLY: u32 = 1u;
const MODE_SCREEN: u32 = 2u;
const MODE_OVERLAY: u32 = 3u;
const MODE_DARKEN: u32 = 4u;
const MODE_LIGHTEN: u32 = 5u;
const MODE_COLOR_DODGE: u32 = 6u;
const MODE_COLOR_BURN: u32 = 7u;
const MODE_HARD_LIGHT: u32 = 8u;
const MODE_SOFT_LIGHT: u32 = 9u;
const MODE_DIFFERENCE: u32 = 10u;
const MODE_EXCLUSION: u32 = 11u;
const MODE_ADD: u32 = 12u;

// True when `v` is a NaN bit pattern. WGSL NaN comparisons are not reliable
// across drivers, so we inspect the raw IEEE-754 bits instead.
fn is_nan_f32(v: f32) -> bool {
    let bits = bitcast<u32>(v);
    return (bits & 0x7f800000u) == 0x7f800000u && (bits & 0x007fffffu) != 0u;
}

fn blend_normal(d: f32, s: f32) -> f32 {
    return s;
}

fn blend_multiply(d: f32, s: f32) -> f32 {
    return d * s;
}

fn blend_screen(d: f32, s: f32) -> f32 {
    return d + s - d * s;
}

fn blend_overlay(d: f32, s: f32) -> f32 {
    if d <= 0.5 {
        return 2.0 * d * s;
    } else {
        return 1.0 - 2.0 * (1.0 - d) * (1.0 - s);
    }
}

fn blend_darken(d: f32, s: f32) -> f32 {
    return min(d, s);
}

fn blend_lighten(d: f32, s: f32) -> f32 {
    return max(d, s);
}

fn blend_color_dodge(d: f32, s: f32) -> f32 {
    if s >= 1.0 {
        return 1.0;
    }
    return clamp(d / (1.0 - s), 0.0, 1.0);
}

fn blend_color_burn(d: f32, s: f32) -> f32 {
    if s <= 0.0 {
        return 0.0;
    }
    return clamp(1.0 - (1.0 - d) / s, 0.0, 1.0);
}

fn blend_hard_light(d: f32, s: f32) -> f32 {
    if s <= 0.5 {
        return 2.0 * d * s;
    } else {
        return 1.0 - 2.0 * (1.0 - d) * (1.0 - s);
    }
}

// W3C SVG soft-light blend.
fn blend_soft_light(d: f32, s: f32) -> f32 {
    let dc = clamp(d, 0.0, 1.0);
    if s <= 0.5 {
        return dc - (1.0 - 2.0 * s) * dc * (1.0 - dc);
    } else {
        return dc + (2.0 * s - 1.0) * (sqrt(dc) - dc);
    }
}

fn blend_difference(d: f32, s: f32) -> f32 {
    return abs(d - s);
}

fn blend_exclusion(d: f32, s: f32) -> f32 {
    return d + s - 2.0 * d * s;
}

fn blend_add(d: f32, s: f32) -> f32 {
    return d + s;
}

fn blend_channel(mode: u32, d: f32, s: f32) -> f32 {
    switch mode {
        case MODE_NORMAL: { return blend_normal(d, s); }
        case MODE_MULTIPLY: { return blend_multiply(d, s); }
        case MODE_SCREEN: { return blend_screen(d, s); }
        case MODE_OVERLAY: { return blend_overlay(d, s); }
        case MODE_DARKEN: { return blend_darken(d, s); }
        case MODE_LIGHTEN: { return blend_lighten(d, s); }
        case MODE_COLOR_DODGE: { return blend_color_dodge(d, s); }
        case MODE_COLOR_BURN: { return blend_color_burn(d, s); }
        case MODE_HARD_LIGHT: { return blend_hard_light(d, s); }
        case MODE_SOFT_LIGHT: { return blend_soft_light(d, s); }
        case MODE_DIFFERENCE: { return blend_difference(d, s); }
        case MODE_EXCLUSION: { return blend_exclusion(d, s); }
        case MODE_ADD: { return blend_add(d, s); }
        default: { return s; }
    }
}

fn blend_rgb(mode: u32, d: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        blend_channel(mode, d.r, s.r),
        blend_channel(mode, d.g, s.g),
        blend_channel(mode, d.b, s.b),
    );
}

// Composite one source texel over the accumulator texel, returning the new
// accumulator value. This is the register-resident equivalent of one
// `blend.wgsl::composite` dispatch: the "leave the destination unchanged" early
// returns of that shader become "return `acc` unchanged" here.
//
// `acc` plays the role of `dst` in the single-layer shader. It always carries a
// finite, [0, 1]-alpha value (the cleared accumulator starts transparent and
// every stored result is normalized), so the NaN/clamp guards on `acc.a` are
// no-ops in practice but are kept to mirror the reference math exactly.
fn composite_over(acc: vec4<f32>, src_in: vec4<f32>, mode: u32, opacity_in: f32, mask_cov_in: f32) -> vec4<f32> {
    var src = src_in;
    // The CPU reference treats NaN source alpha as fully transparent.
    if is_nan_f32(src.a) {
        return acc;
    }

    // The CPU reference treats NaN mask coverage as fully transparent.
    let mask_cov = select(clamp(mask_cov_in, 0.0, 1.0), 0.0, is_nan_f32(mask_cov_in));
    src.a = src.a * mask_cov;

    var dst = acc;
    if is_nan_f32(dst.a) {
        dst.a = 0.0;
    }
    dst.a = clamp(dst.a, 0.0, 1.0);

    let opacity = clamp(opacity_in, 0.0, 1.0);
    var src_a = src.a * opacity;

    // No contribution (including NaN product): leave the accumulator unchanged.
    if src_a <= 0.0 || is_nan_f32(src_a) {
        return dst;
    }
    src_a = min(src_a, 1.0);

    // Transparent accumulator: the result is just the opacity-scaled source.
    if dst.a <= 0.0 {
        return vec4<f32>(src.rgb, src_a);
    }

    let blend_col = blend_rgb(mode, dst.rgb, src.rgb);

    let out_a = src_a + dst.a - src_a * dst.a;
    let out_rgb =
        src.rgb * src_a * (1.0 - dst.a)
        + dst.rgb * dst.a * (1.0 - src_a)
        + src_a * dst.a * blend_col;

    if out_a <= 0.0 {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    return vec4<f32>(out_rgb / out_a, out_a);
}

@compute @workgroup_size(16, 16)
fn composite(@builtin(global_invocation_id) id: vec3<u32>) {
    let dst_dims = textureDimensions(dst_tex);
    if id.x >= dst_dims.x || id.y >= dst_dims.y {
        return;
    }

    let dst_coord = vec2<i32>(i32(id.x), i32(id.y));
    let arr_dims = vec2<i32>(textureDimensions(src_arr));

    // Start from the current accumulator contents. For the first run targeting
    // a freshly cleared tile this is transparent; for a run after a group pop
    // it is the already-composited lower content.
    var acc = textureLoad(dst_tex, dst_coord);

    let n = arrayLength(&layers);
    for (var i = 0u; i < n; i = i + 1u) {
        let p = layers[i];
        let src_coord = dst_coord - vec2<i32>(p.offset_x, p.offset_y);
        if any(src_coord < vec2<i32>(0, 0)) || any(src_coord >= arr_dims) {
            continue;
        }
        let src = textureLoad(src_arr, src_coord, i32(i), 0);
        var cov = 1.0;
        if p.has_mask != 0u {
            cov = textureLoad(mask_arr, src_coord, i32(i), 0).r;
        }
        acc = composite_over(acc, src, p.mode, p.opacity, cov);
    }

    textureStore(dst_tex, dst_coord, acc);
}
