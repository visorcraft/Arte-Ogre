// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

// Blend shader for Arte Ogre.
//
// All inputs and outputs are stored as straight alpha (the CPU reference stores
// pixels in straight alpha). The Porter-Duff "over" operator is applied
// directly on opacity-scaled straight-alpha values, which is mathematically
// equivalent to the premultiplied formulation without an explicit conversion
// step.

// Uniform parameters selecting the blend mode, opacity, and source offset.
struct Params {
    mode: u32,
    opacity: f32,
    // Pixel offset applied to source coordinates: source is read at
    // `dst_coord - src_offset`. Stored as a signed 2-component vector to match
    // the Rust `BlendUniform` layout exactly.
    src_offset: vec2<i32>,
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var dst_tex: texture_storage_2d<rgba32float, read_write>;
@group(0) @binding(2) var<uniform> params: Params;
// Optional layer mask. When no mask is present a tile-sized white texture is
// bound so that `mask.r == 1.0` and the source alpha is unchanged. When the
// layer has a mask but the current source tile has no corresponding mask tile,
// a tile-sized transparent texture is bound so the source is fully masked out.
@group(0) @binding(3) var mask_tex: texture_2d<f32>;

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

// Composite one source texel over the destination accumulator texel.
//
// The source and destination are read and written as straight alpha. Blending
// uses the Porter-Duff "over" operator with a separable blend function applied
// to straight-alpha colors, matching the CPU reference.
//
// Alpha handling matches the CPU reference: the source alpha is multiplied by
// the (already clamped) opacity and the product is capped at 1.0. The mask
// coverage value is clamped to [0, 1] before scaling the source alpha. The
// destination alpha is clamped to [0, 1] but is not premultiplied into the
// source alpha clamp.
@compute @workgroup_size(16, 16)
fn composite(@builtin(global_invocation_id) id: vec3<u32>) {
    let dst_dims = textureDimensions(dst_tex);
    if id.x >= dst_dims.x || id.y >= dst_dims.y {
        return;
    }

    let dst_coord = vec2<i32>(i32(id.x), i32(id.y));
    let src_coord = dst_coord - params.src_offset;

    let src_dims = textureDimensions(src_tex);
    if any(src_coord < vec2<i32>(0, 0)) || any(src_coord >= vec2<i32>(src_dims)) {
        return;
    }

    var src = textureLoad(src_tex, src_coord, 0);

    // The CPU reference treats NaN source alpha as fully transparent.
    if is_nan_f32(src.a) {
        return;
    }

    let mask = textureLoad(mask_tex, src_coord, 0);
    // The CPU reference treats NaN mask coverage as fully transparent.
    let mask_cov = select(clamp(mask.r, 0.0, 1.0), 0.0, is_nan_f32(mask.r));
    src.a = src.a * mask_cov;

    var dst = textureLoad(dst_tex, dst_coord);

    // The CPU reference treats NaN destination alpha as transparent.
    if is_nan_f32(dst.a) {
        dst.a = 0.0;
    }
    // Clamp the destination alpha to [0, 1] to match the CPU reference.
    dst.a = clamp(dst.a, 0.0, 1.0);

    let opacity = clamp(params.opacity, 0.0, 1.0);
    var src_a = src.a * opacity;

    // If the source contributes nothing (including NaN source alpha), leave
    // the accumulator unchanged. The NaN check is performed before `min`
    // because WGSL's `min` returns the non-NaN operand, which would mask the
    // NaN and cause it to propagate into the result.
    if src_a <= 0.0 || is_nan_f32(src_a) {
        return;
    }
    src_a = min(src_a, 1.0);

    // If the accumulator is transparent, the result is just the (already
    // opacity-scaled) source color. Keep it in straight alpha to match the
    // CPU reference storage format.
    if dst.a <= 0.0 {
        textureStore(dst_tex, dst_coord, vec4<f32>(src.rgb, src_a));
        return;
    }

    let blend_col = blend_rgb(params.mode, dst.rgb, src.rgb);

    // Straight-alpha Porter-Duff "over" with a separable blend function.
    let out_a = src_a + dst.a - src_a * dst.a;
    let out_rgb =
        src.rgb * src_a * (1.0 - dst.a)
        + dst.rgb * dst.a * (1.0 - src_a)
        + src_a * dst.a * blend_col;

    if out_a <= 0.0 {
        textureStore(dst_tex, dst_coord, vec4<f32>(0.0, 0.0, 0.0, 0.0));
        return;
    }

    textureStore(dst_tex, dst_coord, vec4<f32>(out_rgb / out_a, out_a));
}
