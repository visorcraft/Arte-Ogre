// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

// Present shader for Arte Ogre.
//
// Samples the linear-space composited result texture and composites it over a
// procedural checkerboard (for transparency visualization) *within the document
// canvas*. Pixels outside the canvas show a flat pasteboard color so the canvas
// reads as a finite sheet rather than an infinite checkerboard. The output
// render target is `Rgba8Unorm`; the shader encodes linear RGB values to sRGB
// before writing so the texture can be registered directly with `egui_wgpu`
// (which requires `Rgba8Unorm` user textures).

struct ViewportUniform {
    pan: vec2<f32>,
    source_origin: vec2<f32>,
    canvas_origin: vec2<f32>,
    canvas_size: vec2<f32>,
    zoom: f32,
    _padding: f32,
};

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_sampler: sampler;
@group(0) @binding(2) var<uniform> viewport: ViewportUniform;

const CHECKER_SIZE: f32 = 16.0;
// sRGB grey shown outside the document canvas (already sRGB-encoded; written
// straight to the `Rgba8Unorm` target).
const PASTEBOARD: f32 = 0.15;

// Encode a linear luminance value to sRGB. This matches the conversion
// performed by an `Rgba8UnormSrgb` render target; by doing it manually we can
// write to the `Rgba8Unorm` format that `egui_wgpu` requires for user textures.
fn linear_to_srgb(linear: f32) -> f32 {
    let v = clamp(linear, 0.0, 1.0);
    if v <= 0.0031308 {
        return v * 12.92;
    } else {
        return 1.055 * pow(v, 1.0 / 2.4) - 0.055;
    }
}

// Porter-Duff "over" operator on straight-alpha values.
fn over(dst: vec4<f32>, src: vec4<f32>) -> vec4<f32> {
    let out_a = src.a + dst.a * (1.0 - src.a);
    if out_a <= 0.0 {
        return vec4<f32>(0.0);
    }
    let out_rgb = (src.rgb * src.a + dst.rgb * dst.a * (1.0 - src.a)) / out_a;
    return vec4<f32>(out_rgb, out_a);
}

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    // A single fullscreen triangle covering NDC space.
    let x = f32(vertex_index % 2u) * 4.0 - 1.0;
    let y = f32(vertex_index / 2u) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

fn checkerboard_color(screen_pos: vec2<f32>) -> vec4<f32> {
    let cx = floor(screen_pos.x / CHECKER_SIZE);
    let cy = floor(screen_pos.y / CHECKER_SIZE);
    let even = (cx + cy) % 2.0 == 0.0;
    var col: vec3<f32>;
    if even {
        col = vec3<f32>(1.0);
    } else {
        // Light grey, not black: the transparency checker must read as "empty",
        // like every other editor. A black square made a cut/erased hole look
        // like a solid black fill.
        col = vec3<f32>(0.8);
    }
    return vec4<f32>(col, 1.0);
}

@fragment
fn fs_main(@builtin(position) frag_pos: vec4<f32>) -> @location(0) vec4<f32> {
    let screen_pos = frag_pos.xy;

    // `pan` is the document-space point currently at the top-left of the
    // canvas. With `screen_pos` measured from the canvas top-left, each screen
    // pixel covers `1/zoom` document pixels.
    let doc_pos = viewport.pan + screen_pos / viewport.zoom;

    // Outside the document canvas: flat pasteboard, no checkerboard and no
    // content, so the canvas does not look infinitely large when the window is
    // bigger than the document.
    let canvas_lo = viewport.canvas_origin;
    let canvas_hi = viewport.canvas_origin + viewport.canvas_size;
    if !(all(doc_pos >= canvas_lo) && all(doc_pos < canvas_hi)) {
        return vec4<f32>(PASTEBOARD, PASTEBOARD, PASTEBOARD, 1.0);
    }

    let src_size = vec2<f32>(textureDimensions(src_tex));
    let src_origin = viewport.source_origin;

    var src = vec4<f32>(0.0);
    if all(doc_pos >= src_origin) && all(doc_pos < src_origin + src_size) {
        let uv = (doc_pos - src_origin) / src_size;
        // Explicit LOD 0 (no mips): valid in the non-uniform control flow above
        // and avoids implicit-derivative artefacts at the canvas edge.
        src = textureSampleLevel(src_tex, src_sampler, uv, 0.0);
    }

    let checker = checkerboard_color(screen_pos);
    let out = over(checker, src);
    // Encode to sRGB here because the output target is `Rgba8Unorm` (required
    // for `egui_wgpu` user-texture registration). Alpha is stored linearly.
    return vec4<f32>(
        linear_to_srgb(out.r),
        linear_to_srgb(out.g),
        linear_to_srgb(out.b),
        out.a,
    );
}
