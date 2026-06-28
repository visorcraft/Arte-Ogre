// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

// In-place adjustment shader for Arte Ogre.
//
// This shader mirrors the CPU reference `apply_adjustment` in
// `ogre_core::compositor`. It reads each pixel from a read/write storage
// texture, applies an adjustment kind with the given opacity, and writes the
// result back to the same texture.

struct Params {
    mode: u32,
    opacity: f32,
    _pad: vec2<f32>,
    param: array<vec4<f32>, 2>,
}

@group(0) @binding(0) var tex: texture_storage_2d<rgba32float, read_write>;
@group(0) @binding(1) var<uniform> params: Params;

const MODE_INVERT: u32 = 0u;
const MODE_DESATURATE: u32 = 1u;
const MODE_BRIGHTNESS_CONTRAST: u32 = 2u;
const MODE_HUE_SAT: u32 = 3u;
const MODE_LEVELS: u32 = 4u;
const MODE_CURVES: u32 = 5u;
const MODE_POSTERIZE: u32 = 6u;
const MODE_THRESHOLD: u32 = 7u;
const MODE_GRADIENT_MAP: u32 = 8u;

fn is_nan_f32(v: f32) -> bool {
    let bits = bitcast<u32>(v);
    return (bits & 0x7f800000u) == 0x7f800000u && (bits & 0x007fffffu) != 0u;
}

fn clamp_channel(v: f32) -> f32 {
    return select(clamp(v, 0.0, 1.0), 0.0, is_nan_f32(v));
}

// Rust's f32::fract() returns a signed fractional part (toward zero), which
// differs from WGSL's fract() for negative values. This matches the CPU code.
fn signed_fract(v: f32) -> f32 {
    return v - trunc(v);
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> vec3<f32> {
    let max = max(max(r, g), b);
    let min = min(min(r, g), b);
    let l = (max + min) * 0.5;
    if max == min {
        return vec3<f32>(0.0, 0.0, l);
    }
    let d = max - min;
    var s: f32;
    if l > 0.5 {
        s = d / (2.0 - max - min);
    } else {
        s = d / (max + min);
    }
    var h: f32;
    if max == r {
        h = ((g - b) / d + select(0.0, 6.0, g < b)) / 6.0;
    } else if max == g {
        h = ((b - r) / d + 2.0) / 6.0;
    } else {
        h = ((r - g) / d + 4.0) / 6.0;
    }
    return vec3<f32>(h, s, l);
}

fn hue_to_rgb(p: f32, q: f32, t: f32) -> f32 {
    var tt = t;
    if tt < 0.0 {
        tt = tt + 1.0;
    }
    if tt > 1.0 {
        tt = tt - 1.0;
    }
    if tt < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * tt;
    }
    if tt < 0.5 {
        return q;
    }
    if tt < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - tt) * 6.0;
    }
    return p;
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> vec3<f32> {
    if s == 0.0 {
        return vec3<f32>(l, l, l);
    }
    var q: f32;
    if l < 0.5 {
        q = l * (1.0 + s);
    } else {
        q = l + s - l * s;
    }
    let p = 2.0 * l - q;
    let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h);
    let b = hue_to_rgb(p, q, h - 1.0 / 3.0);
    return vec3<f32>(r, g, b);
}

fn brightness_contrast(v: f32, br: f32, co: f32) -> f32 {
    return clamp((v - 0.5) * co + 0.5 + br, 0.0, 1.0);
}

fn levels(v: f32, in_black: f32, in_white: f32, out_black: f32, out_white: f32, gamma: f32) -> f32 {
    let in_range = max(in_white - in_black, 1e-6);
    let out_range = out_white - out_black;
    let t = clamp((v - in_black) / in_range, 0.0, 1.0);
    let t2 = pow(t, 1.0 / max(gamma, 1e-6));
    return clamp(out_black + t2 * out_range, 0.0, 1.0);
}

fn curve_map(v: f32, pts: array<vec2<f32>, 4>) -> f32 {
    if (v <= pts[0].x) {
        return pts[0].y;
    }
    for (var i = 0; i < 3; i = i + 1) {
        let a = pts[i];
        let b = pts[i + 1];
        if (v <= b.x) {
            let t = (v - a.x) / max(b.x - a.x, 1e-6);
            return mix(a.y, b.y, clamp(t, 0.0, 1.0));
        }
    }
    return pts[3].y;
}

@compute @workgroup_size(16, 16)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let dims = vec2<u32>(textureDimensions(tex));
    if id.x >= dims.x || id.y >= dims.y {
        return;
    }
    let coord = vec2<i32>(i32(id.x), i32(id.y));
    var c = textureLoad(tex, coord);

    var a = c.a;
    if is_nan_f32(a) {
        a = 0.0;
    }
    a = clamp(a, 0.0, 1.0);
    c.a = a;
    if a == 0.0 {
        textureStore(tex, coord, c);
        return;
    }

    let r = clamp_channel(c.r);
    let g = clamp_channel(c.g);
    let b = clamp_channel(c.b);

    let p0 = params.param[0];
    let p1 = params.param[1];

    var adjusted: vec3<f32>;
    switch params.mode {
        case MODE_INVERT: {
            adjusted = vec3<f32>(1.0 - r, 1.0 - g, 1.0 - b);
        }
        case MODE_DESATURATE: {
            let l = clamp_channel(0.2126 * r + 0.7152 * g + 0.0722 * b);
            adjusted = vec3<f32>(l, l, l);
        }
        case MODE_BRIGHTNESS_CONTRAST: {
            adjusted = vec3<f32>(
                brightness_contrast(r, p0.x, p0.y),
                brightness_contrast(g, p0.x, p0.y),
                brightness_contrast(b, p0.x, p0.y),
            );
        }
        case MODE_HUE_SAT: {
            let hsl = rgb_to_hsl(r, g, b);
            let h = signed_fract(hsl.r + p0.x);
            let s = clamp(hsl.g * p0.y, 0.0, 1.0);
            let l = clamp(hsl.b + p0.z, 0.0, 1.0);
            adjusted = hsl_to_rgb(h, s, l);
        }
        case MODE_LEVELS: {
            adjusted = vec3<f32>(
                levels(r, p0.x, p0.y, p0.z, p0.w, p1.x),
                levels(g, p0.x, p0.y, p0.z, p0.w, p1.x),
                levels(b, p0.x, p0.y, p0.z, p0.w, p1.x),
            );
        }
        case MODE_CURVES: {
            let pts = array<vec2<f32>, 4>(
                vec2<f32>(p0.x, p0.y),
                vec2<f32>(p0.z, p0.w),
                vec2<f32>(p1.x, p1.y),
                vec2<f32>(p1.z, p1.w)
            );
            adjusted = vec3<f32>(curve_map(r, pts), curve_map(g, pts), curve_map(b, pts));
        }
        case MODE_POSTERIZE: {
            let n = max(p0.x, 2.0) - 1.0;
            let pq = vec3<f32>(
                floor(r * n + 0.5) / n,
                floor(g * n + 0.5) / n,
                floor(b * n + 0.5) / n,
            );
            adjusted = clamp(pq, vec3<f32>(0.0), vec3<f32>(1.0));
        }
        case MODE_THRESHOLD: {
            let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            var v = 0.0;
            if (l >= p0.x) { v = 1.0; }
            adjusted = vec3<f32>(v, v, v);
        }
        case MODE_GRADIENT_MAP: {
            let l = clamp(0.2126 * r + 0.7152 * g + 0.0722 * b, 0.0, 1.0);
            let fg = p0.xyz;
            let bg = p1.xyz;
            adjusted = mix(fg, bg, l);
        }
        default: {
            adjusted = vec3<f32>(r, g, b);
        }
    }

    let o = clamp(params.opacity, 0.0, 1.0);
    c.r = r + (adjusted.r - r) * o;
    c.g = g + (adjusted.g - g) * o;
    c.b = b + (adjusted.b - b) * o;
    // Alpha is sanitized but otherwise unchanged.
    textureStore(tex, coord, c);
}
