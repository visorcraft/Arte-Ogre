// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 VisorCraft LLC

// Passthrough compute shader for WASM plugin filters.
//
// Plugins run on the CPU via PluginFilter::apply_cpu, so the GPU path simply
// copies input to output unchanged.

struct Params {
    _dummy: vec4<f32>,
}

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<uniform> dims: vec2<u32>;
@group(0) @binding(2) var<storage, read> input: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> output: array<vec4<f32>>;

@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if id.x < dims.x && id.y < dims.y {
        let idx = id.y * dims.x + id.x;
        output[idx] = input[idx];
    }
    _ = params._dummy;
}
