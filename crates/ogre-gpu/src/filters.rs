// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

//! GPU-accelerated image filters.
//!
//! Filters are described by the [`Filter`] trait.  Each filter supplies its own
//! WGSL compute shader and a parameter buffer; the [`FilterRunner`] dispatches
//! that shader over a dense pixel buffer and returns the result.
//!
//! Every filter also provides a CPU reference implementation so the GPU output
//! can be golden-tested against it.

use crate::context::GpuContext;
use ogre_core::{
    Command, CurvePoint, Document, IVec2, LayerContent, LayerId, OgreError, Rect, Result, Rgba32F,
    TiledBuffer,
};
use std::hash::{Hash, Hasher};
use std::time::Instant;
use wgpu::util::DeviceExt;

/// A buffer of filter parameters uploaded as a WGSL uniform.
///
/// The bytes must match the `Params` struct declared by the filter's WGSL.
/// They are padded to 16-byte alignment, which is sufficient for the
/// std140/std430 layouts used by the built-in filters.
#[derive(Clone, Debug, Default)]
pub struct ParamsBuffer {
    bytes: Vec<u8>,
}

impl ParamsBuffer {
    /// Create a parameter buffer from raw bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Create an empty parameter buffer for filters that take no parameters.
    pub fn empty() -> Self {
        Self { bytes: Vec::new() }
    }

    fn padded(&self) -> Vec<u8> {
        let mut v = self.bytes.clone();
        let pad = (16 - (v.len() % 16)) % 16;
        v.resize(v.len() + pad, 0);
        v
    }
}

/// A GPU filter.
///
/// A filter must provide a WGSL compute shader that declares:
///
/// ```wgsl
/// @group(0) @binding(0) var<uniform> params: Params; // required; may be a dummy
/// @group(0) @binding(1) var<uniform> dims: vec2<u32>;
/// @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
/// @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;
///
/// @compute @workgroup_size(8, 8)
/// fn main(@builtin(global_invocation_id) id: vec3<u32>) { ... }
/// ```
///
/// The dispatch size is `(width + 7) / 8` by `(height + 7) / 8`.
///
/// Parameter-less filters must still declare a `Params` struct at binding 0
/// and touch it (for example with `_ = params._dummy;`) so the binding is not
/// optimized away. The runner binds a 16-byte dummy uniform in that case.
pub trait Filter: Send + Sync + std::fmt::Debug {
    /// Human-readable label for the filter.
    fn label(&self) -> &'static str;

    /// The complete WGSL source for the compute shader.
    fn wgsl(&self) -> &str;

    /// The parameter uniform to bind at group 0, binding 0.
    fn params(&self) -> ParamsBuffer;

    /// CPU reference implementation used to validate the GPU output.
    fn apply_cpu(&self, pixels: &mut [Rgba32F], width: u32);

    /// Apply this filter to a copy of `buffer` and return the filtered buffer.
    ///
    /// This is the shared CPU implementation used both by [`ApplyFilterCmd`]
    /// and by the background plugin worker.
    fn apply_to_tiled_buffer(&self, buffer: &TiledBuffer) -> Result<TiledBuffer> {
        let bounds = buffer.exact_bounds().unwrap_or(Rect::new(0, 0, 0, 0));
        if bounds.is_empty() {
            return Ok(TiledBuffer::new());
        }

        let width = bounds.w;
        let height = bounds.h;
        let mut pixels: Vec<Rgba32F> = Vec::with_capacity((width as usize) * (height as usize));
        for y in 0..height {
            for x in 0..width {
                let local = IVec2::new(bounds.x + x as i32, bounds.y + y as i32);
                pixels.push(buffer.get_pixel(local));
            }
        }

        self.apply_cpu(&mut pixels, width);

        let mut out = TiledBuffer::new();
        for y in 0..height {
            for x in 0..width {
                let local = IVec2::new(bounds.x + x as i32, bounds.y + y as i32);
                let idx = (y as usize) * (width as usize) + (x as usize);
                out.set_pixel(local, pixels[idx]);
            }
        }

        Ok(out)
    }

    /// Number of GPU compute passes required by this filter.
    ///
    /// Most filters are single-pass.  Multi-pass filters may use an internal
    /// ping-pong buffer between passes; the final pass writes to the output.
    fn pass_count(&self) -> u32 {
        1
    }

    /// Parameters for GPU pass `pass` (0-indexed, `< pass_count`).
    ///
    /// Defaults to [`params`](Filter::params) for single-pass filters.
    fn params_for_pass(&self, pass: u32) -> ParamsBuffer {
        let _ = pass;
        self.params()
    }

    /// Whether this filter's parameters describe spatial distances that must be
    /// scaled when previewed on a downsampled proxy.
    fn supports_preview_scaling(&self) -> bool {
        false
    }

    /// Return a version of this filter scaled for a proxy of `scale` relative to
    /// the full-resolution source (`0.0 < scale <= 1.0`).
    ///
    /// Only called when [`supports_preview_scaling`](Filter::supports_preview_scaling)
    /// returns `true`; filters that do not support preview scaling never have
    /// this called and the default implementation panics to surface bugs.
    fn scaled_for_preview(&self, _scale: f32) -> Box<dyn Filter> {
        panic!("scaled_for_preview called on a filter that does not support preview scaling");
    }
}

/// A no-op filter used to verify the filter framework itself.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityFilter;

#[cfg(test)]
impl IdentityFilter {
    /// Create a new identity filter.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
impl Filter for IdentityFilter {
    fn label(&self) -> &'static str {
        "Identity"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                _dummy: u32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                // Touch the params uniform so the binding is not optimized away.
                _ = params._dummy;
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                output[idx] = input[idx];
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::empty()
    }

    fn apply_cpu(&self, _pixels: &mut [Rgba32F], _width: u32) {
        // No-op.
    }
}

/// Invert the RGB channels of an image.
#[derive(Clone, Copy, Debug, Default)]
pub struct InvertFilter;

impl InvertFilter {
    /// Create a new invert filter.
    pub fn new() -> Self {
        Self
    }
}

impl Filter for InvertFilter {
    fn label(&self) -> &'static str {
        "Invert"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                _dummy: u32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                _ = params._dummy;
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                output[idx] = vec4<f32>(1.0 - c.x, 1.0 - c.y, 1.0 - c.z, c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::empty()
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        for p in pixels {
            *p = Rgba32F::new(1.0 - p.r, 1.0 - p.g, 1.0 - p.b, p.a);
        }
    }
}

/// Posterize: quantise each channel to a fixed number of levels.
#[derive(Clone, Copy, Debug)]
pub struct PosterizeFilter {
    /// Number of discrete levels per channel (≥ 2).
    pub levels: f32,
}

impl PosterizeFilter {
    /// Create a new posterize filter with `levels` discrete levels per channel.
    pub fn new(levels: u32) -> Self {
        Self {
            levels: levels.max(2) as f32,
        }
    }
}

impl Filter for PosterizeFilter {
    fn label(&self) -> &'static str {
        "Posterize"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params { levels: f32, }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) { return; }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let n = max(params.levels, 2.0) - 1.0;
                let q = vec3<f32>(
                    floor(c.x * n + 0.5) / n,
                    floor(c.y * n + 0.5) / n,
                    floor(c.z * n + 0.5) / n,
                );
                output[idx] = vec4<f32>(clamp(q, vec3<f32>(0.0), vec3<f32>(1.0)), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(bytemuck::cast_slice(&[self.levels]).to_vec())
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        let n = self.levels.max(2.0) - 1.0;
        for p in pixels {
            let f = |c: f32| (c * n).round() / n;
            *p = Rgba32F::new(f(p.r), f(p.g), f(p.b), p.a);
        }
    }
}

/// Threshold: convert to a binary black/white image at a luminance cutoff.
#[derive(Clone, Copy, Debug)]
pub struct ThresholdFilter {
    /// Luminance cutoff in `0.0..=1.0`; pixels at or above are white.
    pub level: f32,
}

impl ThresholdFilter {
    /// Create a new threshold filter at `level`.
    pub fn new(level: f32) -> Self {
        Self { level }
    }
}

impl Filter for ThresholdFilter {
    fn label(&self) -> &'static str {
        "Threshold"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params { level: f32, }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) { return; }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let l = 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
                var v = 0.0;
                if (l >= params.level) { v = 1.0; }
                output[idx] = vec4<f32>(v, v, v, c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(bytemuck::cast_slice(&[self.level]).to_vec())
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        for p in pixels {
            let l = 0.2126 * p.r + 0.7152 * p.g + 0.0722 * p.b;
            let v = if l >= self.level { 1.0 } else { 0.0 };
            *p = Rgba32F::new(v, v, v, p.a);
        }
    }
}

/// Gradient map: remap each pixel's luminance onto a fg→bg colour ramp.
#[derive(Clone, Copy, Debug)]
pub struct GradientMapFilter {
    /// Colour at luminance 0 (shadow).
    pub fg: Rgba32F,
    /// Colour at luminance 1 (highlight).
    pub bg: Rgba32F,
}

impl GradientMapFilter {
    /// Create a new gradient-map filter.
    pub fn new(fg: Rgba32F, bg: Rgba32F) -> Self {
        Self { fg, bg }
    }
}

impl Filter for GradientMapFilter {
    fn label(&self) -> &'static str {
        "Gradient Map"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                fg: vec4<f32>,
                bg: vec4<f32>,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) { return; }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let l = clamp(0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z, 0.0, 1.0);
                let mapped = mix(params.fg.xyz, params.bg.xyz, l);
                output[idx] = vec4<f32>(mapped, c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(
            bytemuck::cast_slice(&[
                self.fg.r, self.fg.g, self.fg.b, self.fg.a, self.bg.r, self.bg.g, self.bg.b,
                self.bg.a,
            ])
            .to_vec(),
        )
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        for p in pixels {
            let l = (0.2126 * p.r + 0.7152 * p.g + 0.0722 * p.b).clamp(0.0, 1.0);
            let mapped = self.fg.lerp(self.bg, l);
            *p = Rgba32F::new(mapped.r, mapped.g, mapped.b, p.a);
        }
    }
}

/// Desaturate an image using the Rec. 709 luma coefficients.
#[derive(Clone, Copy, Debug, Default)]
pub struct DesaturateFilter;

impl DesaturateFilter {
    /// Create a new desaturate filter.
    pub fn new() -> Self {
        Self
    }
}

impl Filter for DesaturateFilter {
    fn label(&self) -> &'static str {
        "Desaturate"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                _dummy: u32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                _ = params._dummy;
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let l = 0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z;
                output[idx] = vec4<f32>(l, l, l, c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::empty()
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        for p in pixels {
            let l = 0.2126 * p.r + 0.7152 * p.g + 0.0722 * p.b;
            *p = Rgba32F::new(l, l, l, p.a);
        }
    }
}

/// Adjust brightness and contrast.
///
/// Brightness is added to each channel; contrast scales around 0.5.
#[derive(Clone, Copy, Debug)]
pub struct BrightnessContrastFilter {
    /// Brightness offset, typically in `[-1, 1]`.
    pub brightness: f32,
    /// Contrast scale, where `1.0` is unchanged.
    pub contrast: f32,
}

impl BrightnessContrastFilter {
    /// Create a new brightness/contrast filter.
    pub fn new(brightness: f32, contrast: f32) -> Self {
        Self {
            brightness,
            contrast,
        }
    }
}

impl Filter for BrightnessContrastFilter {
    fn label(&self) -> &'static str {
        "Brightness / Contrast"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                brightness: f32,
                contrast: f32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let rgb = (c.xyz - 0.5) * params.contrast + 0.5 + params.brightness;
                output[idx] = vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(bytemuck::cast_slice(&[self.brightness, self.contrast]).to_vec())
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        for p in pixels {
            let f = |x: f32| ((x - 0.5) * self.contrast + 0.5 + self.brightness).clamp(0.0, 1.0);
            *p = Rgba32F::new(f(p.r), f(p.g), f(p.b), p.a);
        }
    }
}

/// Adjust hue, saturation, and lightness.
#[derive(Clone, Copy, Debug)]
pub struct HueSaturationFilter {
    /// Hue shift in degrees.
    pub hue: f32,
    /// Saturation multiplier, where `1.0` is unchanged.
    pub saturation: f32,
    /// Lightness offset, typically in `[-1, 1]`.
    pub lightness: f32,
}

impl HueSaturationFilter {
    /// Create a new hue/saturation/lightness filter.
    pub fn new(hue: f32, saturation: f32, lightness: f32) -> Self {
        Self {
            hue,
            saturation,
            lightness,
        }
    }
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) * 0.5;
    if max == min {
        return (0.0, 0.0, l);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    (h, s, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (f32, f32, f32) {
    if s == 0.0 {
        return (l, l, l);
    }
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h);
    let b = hue_to_rgb(p, q, h - 1.0 / 3.0);
    (r, g, b)
}

fn hue_to_rgb(p: f32, q: f32, t: f32) -> f32 {
    let mut t = t;
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 0.5 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

impl Filter for HueSaturationFilter {
    fn label(&self) -> &'static str {
        "Hue / Saturation"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                hue: f32,
                saturation: f32,
                lightness: f32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            fn rgb_to_hsl(c: vec3<f32>) -> vec3<f32> {
                let mx = max(c.r, max(c.g, c.b));
                let mn = min(c.r, min(c.g, c.b));
                let l = (mx + mn) * 0.5;
                if (mx == mn) {
                    return vec3<f32>(0.0, 0.0, l);
                }
                let d = mx - mn;
                let s = select(d / (mx + mn), d / (2.0 - mx - mn), l > 0.5);
                var h: f32;
                if (mx == c.r) {
                    h = (c.g - c.b) / d + select(6.0, 0.0, c.g >= c.b);
                } else if (mx == c.g) {
                    h = (c.b - c.r) / d + 2.0;
                } else {
                    h = (c.r - c.g) / d + 4.0;
                }
                h = h / 6.0;
                return vec3<f32>(h, s, l);
            }

            fn hue_to_rgb(p: f32, q: f32, t: f32) -> f32 {
                var tt = t;
                if (tt < 0.0) { tt = tt + 1.0; }
                if (tt > 1.0) { tt = tt - 1.0; }
                if (tt < 1.0 / 6.0) { return p + (q - p) * 6.0 * tt; }
                if (tt < 0.5) { return q; }
                if (tt < 2.0 / 3.0) { return p + (q - p) * (2.0 / 3.0 - tt) * 6.0; }
                return p;
            }

            fn hsl_to_rgb(hsl: vec3<f32>) -> vec3<f32> {
                if (hsl.y == 0.0) {
                    return vec3<f32>(hsl.z);
                }
                let q = select(hsl.z + hsl.y - hsl.z * hsl.y, hsl.z * (1.0 + hsl.y), hsl.z < 0.5);
                let p = 2.0 * hsl.z - q;
                let r = hue_to_rgb(p, q, hsl.x + 1.0 / 3.0);
                let g = hue_to_rgb(p, q, hsl.x);
                let b = hue_to_rgb(p, q, hsl.x - 1.0 / 3.0);
                return vec3<f32>(r, g, b);
            }

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                var hsl = rgb_to_hsl(c.xyz);
                hsl.x = fract(hsl.x + params.hue / 360.0);
                hsl.y = clamp(hsl.y * params.saturation, 0.0, 1.0);
                hsl.z = clamp(hsl.z + params.lightness, 0.0, 1.0);
                output[idx] = vec4<f32>(hsl_to_rgb(hsl), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(
            bytemuck::cast_slice(&[self.hue, self.saturation, self.lightness]).to_vec(),
        )
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        for p in pixels {
            let (h, s, l) = rgb_to_hsl(p.r, p.g, p.b);
            let h = (h + self.hue / 360.0).rem_euclid(1.0);
            let s = (s * self.saturation).clamp(0.0, 1.0);
            let l = (l + self.lightness).clamp(0.0, 1.0);
            let (r, g, b) = hsl_to_rgb(h, s, l);
            *p = Rgba32F::new(r, g, b, p.a);
        }
    }
}

/// Adjust tonal range with input/output levels and gamma.
#[derive(Clone, Copy, Debug)]
pub struct LevelsFilter {
    /// Input shadow level.
    pub input_shadow: f32,
    /// Input highlight level.
    pub input_highlight: f32,
    /// Output shadow level.
    pub output_shadow: f32,
    /// Output highlight level.
    pub output_highlight: f32,
    /// Gamma correction applied in the input range.
    pub gamma: f32,
}

impl LevelsFilter {
    /// Create a new levels filter.
    pub fn new(
        input_shadow: f32,
        input_highlight: f32,
        output_shadow: f32,
        output_highlight: f32,
        gamma: f32,
    ) -> Self {
        Self {
            input_shadow,
            input_highlight,
            output_shadow,
            output_highlight,
            gamma,
        }
    }
}

impl Filter for LevelsFilter {
    fn label(&self) -> &'static str {
        "Levels"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                input_shadow: f32,
                input_highlight: f32,
                output_shadow: f32,
                output_highlight: f32,
                gamma: f32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let in_range = max(params.input_highlight - params.input_shadow, 1e-6);
                let out_range = params.output_highlight - params.output_shadow;
                var rgb = (c.xyz - params.input_shadow) / in_range;
                rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
                rgb = pow(rgb, vec3<f32>(1.0 / max(params.gamma, 1e-6)));
                rgb = params.output_shadow + rgb * out_range;
                output[idx] = vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(
            bytemuck::cast_slice(&[
                self.input_shadow,
                self.input_highlight,
                self.output_shadow,
                self.output_highlight,
                self.gamma,
            ])
            .to_vec(),
        )
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        let in_range = (self.input_highlight - self.input_shadow).max(1e-6);
        let out_range = self.output_highlight - self.output_shadow;
        let gamma = self.gamma.max(1e-6);
        for p in pixels {
            let f = |x: f32| {
                let t = ((x - self.input_shadow) / in_range).clamp(0.0, 1.0);
                let t = t.powf(1.0 / gamma);
                (self.output_shadow + t * out_range).clamp(0.0, 1.0)
            };
            *p = Rgba32F::new(f(p.r), f(p.g), f(p.b), p.a);
        }
    }
}

/// Gaussian blur filter.
///
/// `sigma` is the standard deviation of the Gaussian kernel.  The kernel
/// radius is `ceil(3 * sigma)`.
#[derive(Clone, Copy, Debug)]
pub struct GaussianBlurFilter {
    /// Standard deviation of the blur kernel, in pixels.
    pub sigma: f32,
}

impl GaussianBlurFilter {
    /// Create a new Gaussian blur filter.
    pub fn new(sigma: f32) -> Self {
        Self { sigma }
    }

    fn radius(&self) -> i32 {
        if self.sigma < 1e-3 {
            return 0;
        }
        (self.sigma * 3.0).ceil() as i32
    }
}

fn gaussian_weights(sigma: f32, radius: i32) -> Vec<f32> {
    let mut weights: Vec<f32> = (-radius..=radius)
        .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
        .collect();
    let sum: f32 = weights.iter().sum();
    for w in &mut weights {
        *w /= sum;
    }
    weights
}

impl Filter for GaussianBlurFilter {
    fn label(&self) -> &'static str {
        "Gaussian Blur"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                sigma: f32,
                axis: u32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            fn pixel_index(x: i32, y: i32, dims: vec2<u32>) -> i32 {
                let xx = clamp(x, 0, i32(dims.x) - 1);
                let yy = clamp(y, 0, i32(dims.y) - 1);
                return yy * i32(dims.x) + xx;
            }

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                if (params.sigma < 0.001) {
                    let idx = id.y * dims.x + id.x;
                    output[idx] = input[idx];
                    return;
                }
                let radius = i32(ceil(params.sigma * 3.0));
                let ix = i32(id.x);
                let iy = i32(id.y);
                let horizontal = params.axis == 0u;
                var acc = vec4<f32>(0.0);
                var weight_sum = 0.0;
                for (var d = -radius; d <= radius; d = d + 1) {
                    let sx = select(ix, clamp(ix + d, 0, i32(dims.x) - 1), horizontal);
                    let sy = select(clamp(iy + d, 0, i32(dims.y) - 1), iy, horizontal);
                    let w = exp(-f32(d * d) / (2.0 * params.sigma * params.sigma));
                    weight_sum = weight_sum + w;
                    acc = acc + input[pixel_index(sx, sy, dims)] * w;
                }
                output[id.y * dims.x + id.x] = acc / weight_sum;
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        self.params_for_pass(0)
    }

    fn pass_count(&self) -> u32 {
        2
    }

    fn params_for_pass(&self, pass: u32) -> ParamsBuffer {
        let data: [u32; 2] = [self.sigma.to_bits(), pass];
        ParamsBuffer::new(bytemuck::cast_slice(&data).to_vec())
    }

    fn supports_preview_scaling(&self) -> bool {
        true
    }

    fn scaled_for_preview(&self, scale: f32) -> Box<dyn Filter> {
        Box::new(Self::new(self.sigma * scale))
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], width: u32) {
        let radius = self.radius();
        if radius == 0 {
            return;
        }
        let height = (pixels.len() / width as usize) as u32;
        let weights = gaussian_weights(self.sigma, radius);

        // Horizontal pass.
        let mut tmp = pixels.to_vec();
        for y in 0..height {
            for x in 0..width {
                let mut r = 0.0;
                let mut g = 0.0;
                let mut b = 0.0;
                let mut a = 0.0;
                let mut sum = 0.0;
                for (i, w) in weights.iter().enumerate() {
                    let sx = (x as i32 + i as i32 - radius).clamp(0, width as i32 - 1) as u32;
                    let p = pixels[(y * width + sx) as usize];
                    r += p.r * w;
                    g += p.g * w;
                    b += p.b * w;
                    a += p.a * w;
                    sum += w;
                }
                tmp[(y * width + x) as usize] = Rgba32F::new(r / sum, g / sum, b / sum, a / sum);
            }
        }

        // Vertical pass.
        for y in 0..height {
            for x in 0..width {
                let mut r = 0.0;
                let mut g = 0.0;
                let mut b = 0.0;
                let mut a = 0.0;
                let mut sum = 0.0;
                for (i, w) in weights.iter().enumerate() {
                    let sy = (y as i32 + i as i32 - radius).clamp(0, height as i32 - 1) as u32;
                    let p = tmp[(sy * width + x) as usize];
                    r += p.r * w;
                    g += p.g * w;
                    b += p.b * w;
                    a += p.a * w;
                    sum += w;
                }
                pixels[(y * width + x) as usize] = Rgba32F::new(r / sum, g / sum, b / sum, a / sum);
            }
        }
    }
}

/// Sharpen / unsharp mask filter.
///
/// Applies a Gaussian blur and then blends the original with the blur using
/// `result = original + amount * (original - blur)`.
#[derive(Clone, Copy, Debug)]
pub struct SharpenFilter {
    /// Standard deviation of the Gaussian blur used for the mask.
    pub sigma: f32,
    /// Strength of the sharpening effect.
    pub amount: f32,
}

impl SharpenFilter {
    /// Create a new sharpen filter.
    pub fn new(sigma: f32, amount: f32) -> Self {
        Self { sigma, amount }
    }
}

impl Filter for SharpenFilter {
    fn label(&self) -> &'static str {
        "Sharpen"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                sigma: f32,
                amount: f32,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            fn pixel_index(x: i32, y: i32, dims: vec2<u32>) -> i32 {
                let xx = clamp(x, 0, i32(dims.x) - 1);
                let yy = clamp(y, 0, i32(dims.y) - 1);
                return yy * i32(dims.x) + xx;
            }

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                if (params.sigma < 0.001) {
                    output[idx] = c;
                    return;
                }
                let radius = i32(ceil(params.sigma * 3.0));
                let ix = i32(id.x);
                let iy = i32(id.y);
                var blurred = vec4<f32>(0.0);
                var weight_sum = 0.0;
                for (var dy = -radius; dy <= radius; dy = dy + 1) {
                    for (var dx = -radius; dx <= radius; dx = dx + 1) {
                        let d2 = f32(dx * dx + dy * dy);
                        let w = exp(-d2 / (2.0 * params.sigma * params.sigma));
                        weight_sum = weight_sum + w;
                        blurred = blurred + input[pixel_index(ix + dx, iy + dy, dims)] * w;
                    }
                }
                blurred = blurred / weight_sum;
                let result = c.xyz + params.amount * (c.xyz - blurred.xyz);
                output[idx] = vec4<f32>(clamp(result, vec3<f32>(0.0), vec3<f32>(1.0)), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(bytemuck::cast_slice(&[self.sigma, self.amount]).to_vec())
    }

    fn supports_preview_scaling(&self) -> bool {
        true
    }

    fn scaled_for_preview(&self, scale: f32) -> Box<dyn Filter> {
        Box::new(Self::new(self.sigma * scale, self.amount))
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], width: u32) {
        let blur = GaussianBlurFilter::new(self.sigma);
        let mut blurred = pixels.to_vec();
        blur.apply_cpu(&mut blurred, width);
        for (p, b) in pixels.iter_mut().zip(blurred.iter()) {
            let f = |x: f32, y: f32| (x + self.amount * (x - y)).clamp(0.0, 1.0);
            *p = Rgba32F::new(f(p.r, b.r), f(p.g, b.g), f(p.b, b.b), p.a);
        }
    }
}

/// Emboss filter — a directional 3×3 relief convolution.
///
/// Uses the kernel `[[−1,−1, 0], [−1, 1, 1], [ 0, 1, 1]]` (sums to 1, so flat
/// regions are unchanged). The result is blended with the original by `amount`
/// (`0.0` = no change, `1.0` = full relief).
#[derive(Clone, Copy, Debug)]
pub struct EmbossFilter {
    /// Blend strength in `0.0..=1.0`.
    pub amount: f32,
}

impl EmbossFilter {
    /// Create a new emboss filter.
    pub fn new(amount: f32) -> Self {
        Self { amount }
    }
}

impl Filter for EmbossFilter {
    fn label(&self) -> &'static str {
        "Emboss"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params { amount: f32, }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            fn px(x: i32, y: i32, d: vec2<u32>) -> vec4<f32> {
                let xx = clamp(x, 0, i32(d.x) - 1);
                let yy = clamp(y, 0, i32(d.y) - 1);
                return input[yy * i32(d.x) + xx];
            }

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) { return; }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let ix = i32(id.x);
                let iy = i32(id.y);
                // Emboss kernel applied per channel:
                //   -1 -1  0
                //   -1  1  1
                //    0  1  1
                let p00 = px(ix - 1, iy - 1, dims);
                let p10 = px(ix,     iy - 1, dims);
                let p20 = px(ix + 1, iy - 1, dims);
                let p01 = px(ix - 1, iy,     dims);
                let p21 = px(ix + 1, iy,     dims);
                let p02 = px(ix - 1, iy + 1, dims);
                let p12 = px(ix,     iy + 1, dims);
                let p22 = px(ix + 1, iy + 1, dims);
                let emb = -p00 - p10 + p20 - p01 + p21 + p12 + p22 + c;
                let out_rgb = mix(c.xyz, emb.xyz, vec3<f32>(params.amount));
                output[idx] = vec4<f32>(clamp(out_rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(bytemuck::cast_slice(&[self.amount]).to_vec())
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], width: u32) {
        let w = width as i32;
        let h = (pixels.len() / width as usize) as i32;
        let original = pixels.to_vec();
        let clamp_idx = |x: i32, y: i32| {
            let xx = x.clamp(0, w - 1) as usize;
            let yy = y.clamp(0, h - 1) as usize;
            yy * w as usize + xx
        };
        let amount = self.amount;
        for y in 0..h {
            for x in 0..w {
                let i = (y as usize) * (w as usize) + (x as usize);
                let c = original[i];
                let p = |dx: i32, dy: i32| original[clamp_idx(x + dx, y + dy)];
                // Emboss kernel (sums to 1):
                //   -1 -1  0
                //   -1  1  1
                //    0  1  1
                let emb = Rgba32F::new(
                    -p(-1, -1).r - p(0, -1).r + p(1, -1).r - p(-1, 0).r
                        + p(1, 0).r
                        + p(0, 1).r
                        + p(1, 1).r
                        + c.r,
                    -p(-1, -1).g - p(0, -1).g + p(1, -1).g - p(-1, 0).g
                        + p(1, 0).g
                        + p(0, 1).g
                        + p(1, 1).g
                        + c.g,
                    -p(-1, -1).b - p(0, -1).b + p(1, -1).b - p(-1, 0).b
                        + p(1, 0).b
                        + p(0, 1).b
                        + p(1, 1).b
                        + c.b,
                    c.a,
                );
                let blend = |orig: f32, e: f32| (orig + amount * (e - orig)).clamp(0.0, 1.0);
                pixels[i] =
                    Rgba32F::new(blend(c.r, emb.r), blend(c.g, emb.g), blend(c.b, emb.b), c.a);
            }
        }
    }
}

/// Edge-detection filter — Sobel gradient magnitude.
///
/// Computes the Sobel X/Y gradients per channel and takes the magnitude
/// `sqrt(Gx² + Gy²)`, blended with the original by `amount` (`0.0` = no change,
/// `1.0` = edges only).
#[derive(Clone, Copy, Debug)]
pub struct EdgeDetectFilter {
    /// Blend strength in `0.0..=1.0`.
    pub amount: f32,
}

impl EdgeDetectFilter {
    /// Create a new edge-detection filter.
    pub fn new(amount: f32) -> Self {
        Self { amount }
    }
}

impl Filter for EdgeDetectFilter {
    fn label(&self) -> &'static str {
        "Edge Detect"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params { amount: f32, }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

            fn px(x: i32, y: i32, d: vec2<u32>) -> vec4<f32> {
                let xx = clamp(x, 0, i32(d.x) - 1);
                let yy = clamp(y, 0, i32(d.y) - 1);
                return input[yy * i32(d.x) + xx];
            }

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) { return; }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let ix = i32(id.x);
                let iy = i32(id.y);
                // Sobel kernels:
                //   Gx = -1 0 1   Gy = -1 -2 -1
                //        -2 0 2        0  0  0
                //        -1 0 1        1  2  1
                let a = px(ix - 1, iy - 1, dims);
                let b = px(ix + 1, iy - 1, dims);
                let d = px(ix - 1, iy,     dims);
                let e = px(ix + 1, iy,     dims);
                let f = px(ix - 1, iy + 1, dims);
                let g = px(ix + 1, iy + 1, dims);
                let top = px(ix, iy - 1, dims);
                let bot = px(ix, iy + 1, dims);
                let gx = (b - a) + 2.0 * (e - d) + (g - f);
                let gy = (f + 2.0 * top + a) - (g + 2.0 * bot + b);
                let mag = sqrt(gx * gx + gy * gy);
                let out_rgb = mix(c.xyz, mag.xyz, vec3<f32>(params.amount));
                output[idx] = vec4<f32>(clamp(out_rgb, vec3<f32>(0.0), vec3<f32>(1.0)), c.w);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        ParamsBuffer::new(bytemuck::cast_slice(&[self.amount]).to_vec())
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], width: u32) {
        let w = width as i32;
        let h = (pixels.len() / width as usize) as i32;
        let original = pixels.to_vec();
        let clamp_idx = |x: i32, y: i32| {
            let xx = x.clamp(0, w - 1) as usize;
            let yy = y.clamp(0, h - 1) as usize;
            yy * w as usize + xx
        };
        let amount = self.amount;
        for y in 0..h {
            for x in 0..w {
                let i = (y as usize) * (w as usize) + (x as usize);
                let c = original[i];
                let p = |dx: i32, dy: i32| original[clamp_idx(x + dx, y + dy)];
                let channel = |ch: fn(&Rgba32F) -> f32| -> f32 {
                    let gx = (ch(&p(1, -1)) - ch(&p(-1, -1)))
                        + 2.0 * (ch(&p(1, 0)) - ch(&p(-1, 0)))
                        + (ch(&p(1, 1)) - ch(&p(-1, 1)));
                    let gy = (ch(&p(-1, 1)) + 2.0 * ch(&p(0, -1)) + ch(&p(-1, -1)))
                        - (ch(&p(1, 1)) + 2.0 * ch(&p(0, 1)) + ch(&p(1, -1)));
                    (gx * gx + gy * gy).sqrt()
                };
                let mag = Rgba32F::new(channel(|c| c.r), channel(|c| c.g), channel(|c| c.b), c.a);
                let blend = |orig: f32, m: f32| (orig + amount * (m - orig)).clamp(0.0, 1.0);
                pixels[i] =
                    Rgba32F::new(blend(c.r, mag.r), blend(c.g, mag.g), blend(c.b, mag.b), c.a);
            }
        }
    }
}

/// Curves / tone-response filter.
///
/// Maps each RGB channel through a piecewise-linear curve defined by four
/// control points.  The first point is anchored to input `0` and the last to
/// input `1` to keep the curve well-defined across the full range.
#[derive(Clone, Copy, Debug)]
pub struct CurvesFilter {
    /// Four control points sorted by input value.
    pub points: [CurvePoint; 4],
}

impl CurvesFilter {
    /// Create a curves filter from four `(input, output)` pairs.
    ///
    /// The pairs are clamped to `[0, 1]`, sorted by input, and the endpoints
    /// are forced to `(0, output)` and `(1, output)`.
    pub fn new(points: [(f32, f32); 4]) -> Self {
        let mut pts: [CurvePoint; 4] = points.map(|(i, o)| CurvePoint::new(i, o));
        pts.sort_by(|a, b| {
            a.input
                .partial_cmp(&b.input)
                .unwrap_or(core::cmp::Ordering::Equal)
        });
        pts[0].input = 0.0;
        pts[3].input = 1.0;
        Self { points: pts }
    }
}

impl Filter for CurvesFilter {
    fn label(&self) -> &'static str {
        "Curves"
    }

    fn wgsl(&self) -> &str {
        r#"
            struct Params {
                p0: vec4<f32>,
                p1: vec4<f32>,
            }
            @group(0) @binding(0) var<uniform> params: Params;
            @group(0) @binding(1) var<uniform> dims: vec2<u32>;
            @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
            @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

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

            @compute @workgroup_size(8, 8)
            fn main(@builtin(global_invocation_id) id: vec3<u32>) {
                if (id.x >= dims.x || id.y >= dims.y) {
                    return;
                }
                let idx = id.y * dims.x + id.x;
                let c = input[idx];
                let pts = array<vec2<f32>, 4>(
                    vec2<f32>(params.p0.x, params.p0.y),
                    vec2<f32>(params.p0.z, params.p0.w),
                    vec2<f32>(params.p1.x, params.p1.y),
                    vec2<f32>(params.p1.z, params.p1.w)
                );
                let mapped = vec3<f32>(
                    curve_map(c.r, pts),
                    curve_map(c.g, pts),
                    curve_map(c.b, pts)
                );
                output[idx] = vec4<f32>(mapped, c.a);
            }
        "#
    }

    fn params(&self) -> ParamsBuffer {
        let data: [f32; 8] = [
            self.points[0].input,
            self.points[0].output,
            self.points[1].input,
            self.points[1].output,
            self.points[2].input,
            self.points[2].output,
            self.points[3].input,
            self.points[3].output,
        ];
        ParamsBuffer::new(bytemuck::cast_slice(&data).to_vec())
    }

    fn apply_cpu(&self, pixels: &mut [Rgba32F], _width: u32) {
        let map = |v: f32| {
            let pts = self.points;
            if v <= pts[0].input {
                return pts[0].output;
            }
            for i in 0..pts.len() - 1 {
                let a = pts[i];
                let b = pts[i + 1];
                if v <= b.input {
                    let t = (v - a.input) / (b.input - a.input).max(1e-6);
                    return a.output + (b.output - a.output) * t.clamp(0.0, 1.0);
                }
            }
            pts[3].output
        };
        for p in pixels.iter_mut() {
            *p = Rgba32F::new(map(p.r), map(p.g), map(p.b), p.a);
        }
    }
}

/// Apply a filter destructively to a raster layer.
///
/// The command snapshots the layer's previous buffer so undo can restore it
/// exactly.  It applies the filter using the CPU reference implementation to
/// avoid a dependency on a live GPU context in the document history.
#[derive(Debug)]
pub struct ApplyFilterCmd {
    layer: LayerId,
    filter: Box<dyn Filter>,
    old_buffer: Option<TiledBuffer>,
    old_mask: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl ApplyFilterCmd {
    /// Create a destructive filter command.
    pub fn new(layer: LayerId, filter: Box<dyn Filter>) -> Self {
        Self {
            layer,
            filter,
            old_buffer: None,
            old_mask: None,
            prior_active: None,
        }
    }

    /// Return the layer this filter targets.
    pub fn layer_id(&self) -> LayerId {
        self.layer
    }

    /// Consume this command and return the underlying filter.
    ///
    /// This lets the caller run the filter off the UI thread and later dispatch
    /// the result as a [`ReplaceLayerBufferCmd`].
    pub fn into_filter(self) -> Box<dyn Filter> {
        self.filter
    }
}

impl Command for ApplyFilterCmd {
    fn label(&self) -> &'static str {
        self.filter.label()
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }

        self.old_buffer = Some(layer.buffer().unwrap().clone());
        self.old_mask = match &layer.content {
            LayerContent::Raster { mask, .. } => mask.clone(),
            _ => None,
        };
        self.prior_active = doc.active;

        let old = self.old_buffer.as_ref().unwrap();
        let new_buffer = self.filter.apply_to_tiled_buffer(old)?;

        let layer = doc.layer_mut(self.layer)?;
        layer.content = LayerContent::Raster {
            buffer: new_buffer,
            mask: self.old_mask.clone(),
        };
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(buffer) = self.old_buffer.clone() {
            if let Ok(layer) = doc.layer_mut(self.layer) {
                layer.content = LayerContent::Raster {
                    buffer,
                    mask: self.old_mask.clone(),
                };
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// Replace a raster layer's colour buffer with a pre-computed one.
///
/// This is used for off-thread filters: the expensive CPU work is done on a
/// background thread and the UI applies the resulting buffer through the
/// normal command dispatch path, preserving undo/redo history.
#[derive(Debug)]
pub struct ReplaceLayerBufferCmd {
    layer: LayerId,
    new_buffer: TiledBuffer,
    old_buffer: Option<TiledBuffer>,
    old_mask: Option<TiledBuffer>,
    prior_active: Option<LayerId>,
}

impl ReplaceLayerBufferCmd {
    /// Create a command that will replace `layer`'s colour buffer with `buffer`.
    pub fn new(layer: LayerId, buffer: TiledBuffer) -> Self {
        Self {
            layer,
            new_buffer: buffer,
            old_buffer: None,
            old_mask: None,
            prior_active: None,
        }
    }
}

impl Command for ReplaceLayerBufferCmd {
    fn label(&self) -> &'static str {
        "Apply off-thread filter"
    }

    fn apply(&mut self, doc: &mut Document) -> Result<()> {
        let layer = doc.layer(self.layer)?;
        if !layer.is_raster() {
            return Err(OgreError::NotRaster);
        }
        if layer.locked {
            return Err(OgreError::LayerLocked(self.layer));
        }

        // Snapshot the original buffer only the first time we apply. Keeping
        // both snapshots around makes undo/redo cycles safe.
        if self.old_buffer.is_none() {
            self.old_buffer = Some(layer.buffer().unwrap().clone());
            self.old_mask = match &layer.content {
                LayerContent::Raster { mask, .. } => mask.clone(),
                _ => None,
            };
            self.prior_active = doc.active;
        }

        let layer = doc.layer_mut(self.layer)?;
        layer.content = LayerContent::Raster {
            buffer: self.new_buffer.clone(),
            mask: self.old_mask.clone(),
        };
        doc.active = Some(self.layer);
        Ok(())
    }

    fn undo(&mut self, doc: &mut Document) {
        if let Some(buffer) = self.old_buffer.clone() {
            if let Ok(layer) = doc.layer_mut(self.layer) {
                layer.content = LayerContent::Raster {
                    buffer,
                    mask: self.old_mask.clone(),
                };
            }
        }
        doc.active = self.prior_active;
    }

    fn referenced_layers(&self) -> Vec<LayerId> {
        vec![self.layer]
    }
}

/// A downscaled GPU preview of a filter effect.
///
/// Previews are computed on a small proxy so parameter tweaks feel
/// interactive.  The proxy is built with bilinear downsampling and then
/// filtered on the GPU; callers display the returned RGBA buffer as they see
/// fit.
#[derive(Debug)]
pub struct FilterPreview {
    ctx: GpuContext,
    runner: FilterRunner,
}

impl FilterPreview {
    /// Create a preview renderer backed by a new headless GPU context.
    pub fn new() -> Self {
        Self {
            ctx: GpuContext::headless(),
            runner: FilterRunner::new(),
        }
    }

    /// Create a preview renderer using an existing GPU context.
    pub fn with_context(ctx: GpuContext) -> Self {
        Self {
            ctx,
            runner: FilterRunner::new(),
        }
    }

    /// Compute a preview of `filter` applied to `source`.
    ///
    /// The returned buffer is row-major RGBA with dimensions `(width, height)`.
    /// The long edge of the proxy is clamped to `max_dimension`.  An empty
    /// source yields an empty buffer and `(0, 0)`.
    pub fn compute(
        &self,
        filter: &dyn Filter,
        source: &TiledBuffer,
        max_dimension: u32,
    ) -> (Vec<Rgba32F>, u32, u32) {
        let bounds = match source.exact_bounds() {
            Some(b) => b,
            None => return (Vec::new(), 0, 0),
        };
        if bounds.is_empty() || max_dimension == 0 {
            return (Vec::new(), 0, 0);
        }

        let long_edge = bounds.w.max(bounds.h) as f32;
        let scale = (max_dimension as f32 / long_edge).min(1.0);
        let proxy_w = ((bounds.w as f32 * scale).ceil() as u32).max(1);
        let proxy_h = ((bounds.h as f32 * scale).ceil() as u32).max(1);

        // Map source bounds to a (0,0)-anchored proxy of the computed size.
        let affine = glam::Affine2::from_scale_angle_translation(
            glam::Vec2::splat(scale),
            0.0,
            glam::Vec2::new(-bounds.x as f32 * scale, -bounds.y as f32 * scale),
        );
        let proxy =
            ogre_core::resample::resample(source, affine, ogre_core::resample::Filter::Bilinear);

        let mut input: Vec<Rgba32F> = Vec::with_capacity((proxy_w as usize) * (proxy_h as usize));
        for y in 0..proxy_h {
            for x in 0..proxy_w {
                input.push(proxy.get_pixel(IVec2::new(x as i32, y as i32)));
            }
        }

        let scaled = if filter.supports_preview_scaling() {
            Some(filter.scaled_for_preview(scale))
        } else {
            None
        };
        let preview_filter: &dyn Filter = scaled.as_deref().unwrap_or(filter);
        let output = self
            .runner
            .run_dyn(&self.ctx, preview_filter, &input, proxy_w, proxy_h)
            .unwrap_or_default();
        (output, proxy_w, proxy_h)
    }
}

impl Default for FilterPreview {
    fn default() -> Self {
        Self::new()
    }
}

/// Maximum number of compute pipelines cached by a [`FilterRunner`].
///
/// This bounds GPU memory and descriptor growth when many unique filters
/// (e.g. plugin-generated shaders with baked-in parameters) are run in one
/// session. The limit is well above the number of built-in filters and is
/// enforced with LRU eviction.
const MAX_PIPELINES: usize = 64;

/// A cached compute pipeline with LRU metadata.
#[derive(Debug)]
struct CachedPipeline {
    pipeline: wgpu::ComputePipeline,
    last_used: Instant,
}

/// Runs a [`Filter`] on the GPU and returns the resulting pixel buffer.
///
/// Pipelines are cached by the filter's [`label`](Filter::label) and the hash
/// of its WGSL source, so repeated runs of the same filter type (e.g. live
/// previews) do not recompile WGSL. The cache has a bounded size and evicts
/// least-recently-used entries when the limit is exceeded.
#[derive(Debug)]
pub struct FilterRunner {
    pipelines: std::sync::Mutex<ahash::AHashMap<(&'static str, u64), CachedPipeline>>,
}

impl FilterRunner {
    /// Create a new filter runner.
    pub fn new() -> Self {
        Self {
            pipelines: std::sync::Mutex::new(ahash::AHashMap::new()),
        }
    }

    /// Run a dynamic filter on `input` and return the output.
    pub fn run_dyn(
        &self,
        ctx: &GpuContext,
        filter: &dyn Filter,
        input: &[Rgba32F],
        width: u32,
        height: u32,
    ) -> core::result::Result<Vec<Rgba32F>, OgreError> {
        assert_eq!(
            input.len(),
            (width as usize) * (height as usize),
            "input size must match width * height"
        );

        let device = &ctx.device;
        let queue = &ctx.queue;

        let label = filter.label();
        let wgsl = filter.wgsl();
        let source_hash = {
            let mut hasher = ahash::AHasher::default();
            wgsl.hash(&mut hasher);
            hasher.finish()
        };
        let pipeline = {
            let mut cache = self.pipelines.lock().unwrap();
            let now = Instant::now();
            let key = (label, source_hash);
            let pipeline = cache
                .entry(key)
                .and_modify(|e| e.last_used = now)
                .or_insert_with(|| {
                    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some(label),
                        source: wgpu::ShaderSource::Wgsl(wgsl.into()),
                    });
                    let pipeline =
                        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                            label: Some(label),
                            layout: None,
                            module: &shader,
                            entry_point: Some("main"),
                            compilation_options: wgpu::PipelineCompilationOptions::default(),
                            cache: None,
                        });
                    CachedPipeline {
                        pipeline,
                        last_used: now,
                    }
                })
                .pipeline
                .clone();

            // Enforce a bounded pipeline cache so plugin-generated or preview
            // filters cannot grow GPU memory without limit.
            if cache.len() > MAX_PIPELINES {
                let to_evict = cache.len() - MAX_PIPELINES;
                let mut by_age: Vec<((&'static str, u64), Instant)> =
                    cache.iter().map(|(&k, v)| (k, v.last_used)).collect();
                by_age.sort_by_key(|(_, t)| *t);
                for (key, _) in by_age.into_iter().take(to_evict) {
                    cache.remove(&key);
                }
            }

            pipeline
        };

        // Upload input as vec4<f32>.
        let input_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("filter input"),
            contents: bytemuck::cast_slice(input),
            usage: wgpu::BufferUsages::STORAGE,
        });

        let output_size = (input.len() * 16) as wgpu::BufferAddress;
        let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("filter output"),
            size: output_size,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });

        let pass_count = filter.pass_count().max(1) as usize;

        // Intermediate ping-pong buffer for multi-pass filters.
        let intermediate: Option<wgpu::Buffer> = if pass_count > 1 {
            Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("filter intermediate"),
                size: output_size,
                usage: wgpu::BufferUsages::STORAGE,
                mapped_at_creation: false,
            }))
        } else {
            None
        };

        // Dims uniform.
        let dims = [width, height, 0u32, 0u32];
        let dims_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("filter dims"),
            contents: bytemuck::cast_slice(&dims),
            usage: wgpu::BufferUsages::UNIFORM,
        });

        let bind_group_layout = pipeline.get_bind_group_layout(0);
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("filter encoder"),
        });

        let mut current_input: &wgpu::Buffer = &input_buffer;
        for pass in 0..pass_count {
            let is_last = pass == pass_count - 1;
            let current_output: &wgpu::Buffer = if is_last {
                &output_buffer
            } else {
                intermediate
                    .as_ref()
                    .expect("intermediate buffer for multi-pass filter")
            };

            // Params uniform for this pass (may be empty).
            let params_padded = filter.params_for_pass(pass as u32).padded();
            let params_buffer = if params_padded.is_empty() {
                // Bind group requires a non-zero-sized buffer; use a dummy 16-byte
                // buffer for parameter-less filters.
                device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("filter params dummy"),
                    contents: &[0u8; 16],
                    usage: wgpu::BufferUsages::UNIFORM,
                })
            } else {
                device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("filter params"),
                    contents: &params_padded,
                    usage: wgpu::BufferUsages::UNIFORM,
                })
            };

            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("filter bind group"),
                layout: &bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: dims_buffer.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: current_input.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: current_output.as_entire_binding(),
                    },
                ],
            });

            {
                let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: Some("filter pass"),
                    timestamp_writes: None,
                });
                cpass.set_pipeline(&pipeline);
                cpass.set_bind_group(0, &bind_group, &[]);
                cpass.dispatch_workgroups(width.div_ceil(8), height.div_ceil(8), 1);
            }

            current_input = current_output;
        }

        // Readback buffer.
        let readback_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("filter readback"),
            size: output_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_buffer_to_buffer(&output_buffer, 0, &readback_buffer, 0, output_size);
        queue.submit(Some(encoder.finish()));

        // Map and convert back to Rgba32F.
        let buffer_slice = readback_buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .map_err(|_| OgreError::FilterFailed("device poll failed"))?;
        // Propagate a mapping failure as an error rather than panicking in
        // `get_mapped_range` below.
        match rx.recv() {
            Ok(Ok(())) => {}
            _ => return Err(OgreError::FilterFailed("buffer map failed")),
        }

        let output = {
            let data = buffer_slice.get_mapped_range();
            let floats: &[f32] = bytemuck::cast_slice(&data);
            floats
                .chunks_exact(4)
                .map(|chunk| Rgba32F::new(chunk[0], chunk[1], chunk[2], chunk[3]))
                .collect()
        };
        readback_buffer.unmap();
        Ok(output)
    }
}

impl Default for FilterRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_filter_matches_cpu_reference() {
        let ctx = GpuContext::headless();
        let runner = FilterRunner::new();
        let filter = IdentityFilter::new();

        let width = 16;
        let height = 8;
        let mut input: Vec<Rgba32F> = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = x as f32 / width as f32;
                let g = y as f32 / height as f32;
                input.push(Rgba32F::new(r, g, 0.5, 1.0));
            }
        }

        let mut cpu = input.clone();
        filter.apply_cpu(&mut cpu, width);

        let gpu = runner
            .run_dyn(&ctx, &filter, &input, width, height)
            .unwrap();
        assert_eq!(cpu.len(), gpu.len());
        for (a, b) in cpu.iter().zip(gpu.iter()) {
            assert!((a.r - b.r).abs() < 1e-4);
            assert!((a.g - b.g).abs() < 1e-4);
            assert!((a.b - b.b).abs() < 1e-4);
            assert!((a.a - b.a).abs() < 1e-4);
        }
    }

    #[test]
    fn identity_filter_handles_non_multiple_workgroup_size() {
        let ctx = GpuContext::headless();
        let runner = FilterRunner::new();
        let filter = IdentityFilter::new();

        let width = 15;
        let height = 7;
        let input: Vec<Rgba32F> = (0..(width * height))
            .map(|i| Rgba32F::new(i as f32, 0.0, 0.0, 1.0))
            .collect();

        let gpu = runner
            .run_dyn(&ctx, &filter, &input, width, height)
            .unwrap();
        assert_eq!(gpu, input);
    }

    fn golden_test<F: Filter>(filter: &F, width: u32, height: u32) {
        let ctx = GpuContext::headless();
        let runner = FilterRunner::new();

        let mut input: Vec<Rgba32F> = Vec::with_capacity((width * height) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = x as f32 / width.max(1) as f32;
                let g = y as f32 / height.max(1) as f32;
                input.push(Rgba32F::new(r, g, 0.5, 0.8));
            }
        }

        let mut cpu = input.clone();
        filter.apply_cpu(&mut cpu, width);
        let gpu = runner.run_dyn(&ctx, filter, &input, width, height).unwrap();

        assert_eq!(cpu.len(), gpu.len());
        for (a, b) in cpu.iter().zip(gpu.iter()) {
            assert!(
                (a.r - b.r).abs() < 1e-4
                    && (a.g - b.g).abs() < 1e-4
                    && (a.b - b.b).abs() < 1e-4
                    && (a.a - b.a).abs() < 1e-4,
                "CPU {:?} != GPU {:?}",
                a,
                b
            );
        }
    }

    #[test]
    fn invert_filter_matches_cpu() {
        golden_test(&InvertFilter::new(), 13, 11);
    }

    #[test]
    fn desaturate_filter_matches_cpu() {
        golden_test(&DesaturateFilter::new(), 13, 11);
    }

    #[test]
    fn brightness_contrast_filter_matches_cpu() {
        golden_test(&BrightnessContrastFilter::new(0.1, 1.2), 13, 11);
    }

    #[test]
    fn hue_saturation_filter_matches_cpu() {
        golden_test(&HueSaturationFilter::new(90.0, 1.3, -0.1), 13, 11);
    }

    #[test]
    fn levels_filter_matches_cpu() {
        golden_test(&LevelsFilter::new(0.1, 0.9, 0.0, 1.0, 1.2), 13, 11);
    }

    #[test]
    fn gaussian_blur_filter_matches_cpu() {
        golden_test(&GaussianBlurFilter::new(1.5), 16, 12);
    }

    #[test]
    fn gaussian_blur_large_radius_matches_cpu() {
        // Large radius stresses edge clamping and separable-pass equivalence.
        golden_test(&GaussianBlurFilter::new(5.0), 32, 24);
    }

    #[test]
    fn sharpen_filter_matches_cpu() {
        golden_test(&SharpenFilter::new(1.0, 1.5), 16, 12);
    }

    #[test]
    fn emboss_filter_matches_cpu() {
        golden_test(&EmbossFilter::new(1.0), 16, 12);
    }

    #[test]
    fn emboss_filter_partial_amount_matches_cpu() {
        golden_test(&EmbossFilter::new(0.4), 13, 11);
    }

    #[test]
    fn edge_detect_filter_matches_cpu() {
        golden_test(&EdgeDetectFilter::new(1.0), 16, 12);
    }

    #[test]
    fn edge_detect_filter_partial_amount_matches_cpu() {
        golden_test(&EdgeDetectFilter::new(0.5), 13, 11);
    }

    #[test]
    fn posterize_filter_matches_cpu() {
        golden_test(&PosterizeFilter::new(4), 16, 12);
    }

    #[test]
    fn threshold_filter_matches_cpu() {
        golden_test(&ThresholdFilter::new(0.5), 16, 12);
    }

    #[test]
    fn gradient_map_filter_matches_cpu() {
        golden_test(
            &GradientMapFilter::new(
                Rgba32F::new(0.1, 0.0, 0.2, 1.0),
                Rgba32F::new(1.0, 0.8, 0.2, 1.0),
            ),
            16,
            12,
        );
    }

    fn preview_parity_test(filter: &dyn Filter, tolerance: f32) {
        let ctx = GpuContext::headless();
        let preview = FilterPreview::with_context(ctx);

        // 256x192 source with a simple gradient; proxy long edge clamped to 64.
        let src_w = 256u32;
        let src_h = 192u32;
        let max_dim = 64u32;
        let mut source = TiledBuffer::new();
        for y in 0..src_h {
            for x in 0..src_w {
                source.set_pixel(
                    IVec2::new(x as i32, y as i32),
                    Rgba32F::new(x as f32 / src_w as f32, y as f32 / src_h as f32, 0.5, 1.0),
                );
            }
        }

        let (proxy, pw, ph) = preview.compute(filter, &source, max_dim);
        assert!(pw <= max_dim && ph <= max_dim);
        assert!(!proxy.is_empty());

        // Full-res CPU result, then downscale to the same proxy size.
        let bounds = source.exact_bounds().unwrap();
        let mut full: Vec<Rgba32F> = Vec::with_capacity((bounds.w * bounds.h) as usize);
        for y in 0..bounds.h {
            for x in 0..bounds.w {
                full.push(source.get_pixel(IVec2::new(x as i32, y as i32)));
            }
        }
        filter.apply_cpu(&mut full, bounds.w);

        let mut full_buf = TiledBuffer::new();
        for y in 0..bounds.h {
            for x in 0..bounds.w {
                full_buf.set_pixel(
                    IVec2::new(x as i32, y as i32),
                    full[(y * bounds.w + x) as usize],
                );
            }
        }

        let scale = pw as f32 / bounds.w as f32;
        let affine = glam::Affine2::from_scale_angle_translation(
            glam::Vec2::splat(scale),
            0.0,
            glam::Vec2::ZERO,
        );
        let reference =
            ogre_core::resample::resample(&full_buf, affine, ogre_core::resample::Filter::Bilinear);

        for y in 0..ph {
            for x in 0..pw {
                let a = proxy[(y * pw + x) as usize];
                let b = reference.get_pixel(IVec2::new(x as i32, y as i32));
                assert!(
                    (a.r - b.r).abs() < tolerance
                        && (a.g - b.g).abs() < tolerance
                        && (a.b - b.b).abs() < tolerance
                        && (a.a - b.a).abs() < tolerance,
                    "mismatch at ({},{}): preview {:?} vs reference {:?}",
                    x,
                    y,
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn preview_matches_full_res_for_local_filter() {
        preview_parity_test(&InvertFilter::new(), 1e-3);
    }

    #[test]
    fn preview_matches_full_res_for_spatial_filters() {
        // Spatial filters are previewed on a downsampled proxy; the scaled
        // kernel can only approximate the full-res result near image edges
        // because of discretisation and clamping, so a looser tolerance is used.
        preview_parity_test(&GaussianBlurFilter::new(2.0), 2e-2);
        preview_parity_test(&SharpenFilter::new(1.5, 1.2), 2e-2);
    }

    #[test]
    fn apply_filter_cmd_writes_and_undo_restores_pixels() {
        use ogre_core::{Document, Rgba32F};

        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        doc.layer_mut(id)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(0.2, 0.4, 0.6, 1.0));

        let mut cmd = ApplyFilterCmd::new(id, Box::new(InvertFilter::new()));
        cmd.apply(&mut doc).unwrap();
        let after = doc
            .layer(id)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(0, 0));
        assert!(
            (after.r - 0.8).abs() < 1e-4
                && (after.g - 0.6).abs() < 1e-4
                && (after.b - 0.4).abs() < 1e-4
                && (after.a - 1.0).abs() < 1e-4,
            "unexpected inverted pixel {:?}",
            after
        );

        cmd.undo(&mut doc);
        let restored = doc
            .layer(id)
            .unwrap()
            .buffer()
            .unwrap()
            .get_pixel(IVec2::new(0, 0));
        assert!(
            (restored.r - 0.2).abs() < 1e-4
                && (restored.g - 0.4).abs() < 1e-4
                && (restored.b - 0.6).abs() < 1e-4
                && (restored.a - 1.0).abs() < 1e-4,
            "unexpected restored pixel {:?}",
            restored
        );
    }

    #[test]
    fn apply_filter_cmd_preserves_mask() {
        use ogre_core::{Document, Rgba32F};

        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        let layer = doc.layer_mut(id).unwrap();
        layer
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let mut mask = ogre_core::TiledBuffer::new();
        mask.set_pixel(IVec2::new(0, 0), Rgba32F::new(0.5, 0.0, 0.0, 1.0));
        layer.content = ogre_core::LayerContent::Raster {
            buffer: layer.buffer().unwrap().clone(),
            mask: Some(mask),
        };

        let mut cmd = ApplyFilterCmd::new(id, Box::new(InvertFilter::new()));
        cmd.apply(&mut doc).unwrap();
        let layer = doc.layer(id).unwrap();
        let inverted = layer.buffer().unwrap().get_pixel(IVec2::new(0, 0));
        assert!(inverted.r < 0.1 && inverted.g > 0.9 && inverted.b > 0.9);
        assert!(layer.mask().is_some());
        assert!((layer.mask().unwrap().get_pixel(IVec2::new(0, 0)).r - 0.5).abs() < 1e-4);

        cmd.undo(&mut doc);
        let layer = doc.layer(id).unwrap();
        assert!(layer.mask().is_some());
        assert!((layer.mask().unwrap().get_pixel(IVec2::new(0, 0)).r - 0.5).abs() < 1e-4);
        let restored = layer.buffer().unwrap().get_pixel(IVec2::new(0, 0));
        assert!(restored.r > 0.9 && restored.g < 0.1 && restored.b < 0.1);
    }

    #[test]
    fn apply_filter_cmd_rejects_non_raster_and_locked() {
        use ogre_core::{AdjustmentKind, Document, OgreError};

        let mut doc = Document::new(10, 10);
        let adjustment = doc.add_adjustment_layer("A", AdjustmentKind::Invert);
        let raster = doc.add_raster_layer("L");
        doc.layer_mut(raster).unwrap().locked = true;

        let mut cmd = ApplyFilterCmd::new(adjustment, Box::new(InvertFilter::new()));
        assert!(matches!(
            cmd.apply(&mut doc).unwrap_err(),
            OgreError::NotRaster
        ));

        let mut cmd = ApplyFilterCmd::new(raster, Box::new(InvertFilter::new()));
        assert!(matches!(
            cmd.apply(&mut doc).unwrap_err(),
            OgreError::LayerLocked(_)
        ));
    }

    #[test]
    fn apply_filter_cmd_empty_bounds_is_no_op_and_undo_restores() {
        use ogre_core::Document;

        let mut doc = Document::new(10, 10);
        let id = doc.add_raster_layer("L");
        let prior_active = doc.active;

        let mut cmd = ApplyFilterCmd::new(id, Box::new(InvertFilter::new()));
        cmd.apply(&mut doc).unwrap();
        assert_eq!(doc.active, Some(id));

        cmd.undo(&mut doc);
        assert_eq!(doc.active, prior_active);
        assert_eq!(cmd.referenced_layers(), vec![id]);
    }

    #[test]
    fn hue_saturation_negative_degrees_wraps_like_wgsl() {
        // Red rotated by -120 degrees should become blue (HSL hue ≈ 2/3).
        let ctx = GpuContext::headless();
        let runner = FilterRunner::new();
        let input = vec![Rgba32F::new(1.0, 0.0, 0.0, 1.0)];
        let gpu = runner
            .run_dyn(
                &ctx,
                &HueSaturationFilter::new(-120.0, 1.0, 0.0),
                &input,
                1,
                1,
            )
            .unwrap();
        let mut cpu = input.clone();
        HueSaturationFilter::new(-120.0, 1.0, 0.0).apply_cpu(&mut cpu, 1);
        assert!((gpu[0].r - cpu[0].r).abs() < 1e-4);
        assert!((gpu[0].g - cpu[0].g).abs() < 1e-4);
        assert!((gpu[0].b - cpu[0].b).abs() < 1e-4);
        // Blue channel should dominate.
        assert!(gpu[0].b > gpu[0].r + 0.3 && gpu[0].b > gpu[0].g + 0.3);
    }

    #[test]
    fn filter_matches_adjustment_kind_in_compositor() {
        use ogre_core::{compositor, AdjustmentKind, Document, Rgba32F};

        let mut doc = Document::new(4, 4);
        let layer = doc.add_raster_layer("L");
        doc.layer_mut(layer)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(
                ogre_core::IVec2::new(0, 0),
                Rgba32F::new(0.2, 0.4, 0.6, 1.0),
            );
        doc.add_adjustment_layer("adj", AdjustmentKind::Invert);

        let region = ogre_core::Rect::new(0, 0, doc.canvas.w, doc.canvas.h);
        let composite = compositor::composite_document(&doc, region).unwrap();
        let from_adj = composite[0];

        // Direct filter on the same pixel.
        let mut pixels = vec![Rgba32F::new(0.2, 0.4, 0.6, 1.0)];
        InvertFilter::new().apply_cpu(&mut pixels, 1);

        assert!((from_adj.r - pixels[0].r).abs() < 1e-4);
        assert!((from_adj.g - pixels[0].g).abs() < 1e-4);
        assert!((from_adj.b - pixels[0].b).abs() < 1e-4);
    }

    #[test]
    fn preview_empty_source_returns_empty() {
        let ctx = GpuContext::headless();
        let preview = FilterPreview::with_context(ctx);
        let source = TiledBuffer::new();
        let (pixels, w, h) = preview.compute(&InvertFilter::new(), &source, 256);
        assert!(pixels.is_empty());
        assert_eq!((w, h), (0, 0));

        let mut source = TiledBuffer::new();
        source.set_pixel(IVec2::new(0, 0), Rgba32F::new(1.0, 0.0, 0.0, 1.0));
        let (pixels, w, h) = preview.compute(&InvertFilter::new(), &source, 0);
        assert!(pixels.is_empty());
        assert_eq!((w, h), (0, 0));
    }

    #[test]
    fn curves_filter_matches_cpu() {
        golden_test(
            &CurvesFilter::new([(0.0, 0.0), (0.25, 0.1), (0.75, 0.9), (1.0, 1.0)]),
            13,
            11,
        );
    }

    #[test]
    fn gaussian_blur_zero_radius_matches_identity() {
        golden_test(&GaussianBlurFilter::new(0.0), 16, 12);
    }

    /// Test filter with a unique WGSL source per instance so each run creates
    /// a distinct cache key. Used to verify the pipeline cache is bounded.
    #[derive(Debug)]
    struct UniqueFilter {
        wgsl: &'static str,
    }

    impl UniqueFilter {
        fn new(id: u32) -> Self {
            let wgsl = format!(
                r#"
                    struct Params {{
                        _dummy: u32,
                    }}
                    @group(0) @binding(0) var<uniform> params: Params;
                    @group(0) @binding(1) var<uniform> dims: vec2<u32>;
                    @group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
                    @group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

                    @compute @workgroup_size(8, 8)
                    fn main(@builtin(global_invocation_id) id: vec3<u32>) {{
                        // unique-id: {id}
                        _ = params._dummy;
                        if (id.x >= dims.x || id.y >= dims.y) {{
                            return;
                        }}
                        let idx = id.y * dims.x + id.x;
                        output[idx] = input[idx];
                    }}
                "#
            );
            Self {
                wgsl: Box::leak(wgsl.into_boxed_str()),
            }
        }
    }

    impl Filter for UniqueFilter {
        fn label(&self) -> &'static str {
            "Unique"
        }

        fn wgsl(&self) -> &str {
            self.wgsl
        }

        fn params(&self) -> ParamsBuffer {
            ParamsBuffer::empty()
        }

        fn apply_cpu(&self, _pixels: &mut [Rgba32F], _width: u32) {
            // No-op.
        }
    }

    #[test]
    fn pipeline_cache_is_bounded() {
        let ctx = GpuContext::headless();
        let runner = FilterRunner::new();
        let input = vec![Rgba32F::new(0.5, 0.5, 0.5, 1.0); 16];

        for i in 0..(MAX_PIPELINES + 10) as u32 {
            let filter = UniqueFilter::new(i);
            let _ = runner.run_dyn(&ctx, &filter, &input, 4, 4).unwrap();
        }

        let cache_size = runner.pipelines.lock().unwrap().len();
        assert!(
            cache_size <= MAX_PIPELINES,
            "pipeline cache grew beyond MAX_PIPELINES: {cache_size} > {MAX_PIPELINES}"
        );
    }

    #[test]
    fn replace_layer_buffer_cmd_undo_redo_restores_original() {
        let mut doc = Document::new(16, 16);
        let layer = doc.add_raster_layer("test");
        doc.active = Some(layer);

        let original = Rgba32F::new(1.0, 0.0, 0.0, 1.0);
        let changed = Rgba32F::new(0.0, 1.0, 0.0, 1.0);
        doc.layer_mut(layer)
            .unwrap()
            .buffer_mut()
            .unwrap()
            .set_pixel(IVec2::new(3, 4), original);

        let mut new_buffer = TiledBuffer::new();
        new_buffer.set_pixel(IVec2::new(3, 4), changed);

        let mut cmd = ReplaceLayerBufferCmd::new(layer, new_buffer);
        cmd.apply(&mut doc).unwrap();
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(3, 4)),
            changed
        );

        cmd.undo(&mut doc);
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(3, 4)),
            original
        );

        cmd.apply(&mut doc).unwrap();
        assert_eq!(
            doc.layer(layer)
                .unwrap()
                .buffer()
                .unwrap()
                .get_pixel(IVec2::new(3, 4)),
            changed
        );
    }
}
