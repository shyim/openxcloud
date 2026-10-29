// AMD FidelityFX Super Resolution 1.0 - RCAS (Robust Contrast Adaptive Sharpening)
//
// WGSL port of `FsrRcasF` from ffx_fsr1.h (FidelityFX-FSR, AMD, 2021).
//
// Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.
//
// RCAS is a low-pass-limited sharpener that runs on the EASU output. The 5-tap "+"
// pattern (b above, d left, e centre, f right, h below) is loaded with clamped
// `textureLoad`s (the reference uses unclamped `texelFetch`).
//
// `con.x` is `exp2(-sharpness)` as computed by `FsrRcasCon`, with `sharpness` in stops:
// 0.0 = maximum sharpening, 2.0 = barely any. `con.y` != 0 enables the `FSR_RCAS_DENOISE`
// path, which damps sharpening on noisy (e.g. compression-noise) regions.

struct RcasParams {
    con: vec4<f32>,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> c: RcasParams;

// This is set at the limit of providing unnatural results for sharpening.
const FSR_RCAS_LIMIT: f32 = 0.25 - (1.0 / 16.0);

// APrxMedRcpF1
fn aprx_med_rcp(a: f32) -> f32 {
    let b = bitcast<f32>(0x7ef19fffu - bitcast<u32>(a));
    return b * (-b * a + 2.0);
}

fn min3f(x: f32, y: f32, z: f32) -> f32 {
    return min(x, min(y, z));
}

fn max3f(x: f32, y: f32, z: f32) -> f32 {
    return max(x, max(y, z));
}

fn load_src(p: vec2<i32>, size: vec2<i32>) -> vec3<f32> {
    let q = clamp(p, vec2<i32>(0, 0), size - vec2<i32>(1, 1));
    return textureLoad(src, q, 0).rgb;
}

// FsrRcasF
@compute @workgroup_size(16, 16, 1)
fn rcas(@builtin(global_invocation_id) gid: vec3<u32>) {
    let out_size = textureDimensions(dst);
    if (gid.x >= out_size.x || gid.y >= out_size.y) {
        return;
    }
    let size = vec2<i32>(textureDimensions(src));
    let sp = vec2<i32>(gid.xy);

    // Algorithm uses minimal 3x3 pixel neighborhood.
    //    b
    //  d e f
    //    h
    let b = load_src(sp + vec2<i32>(0, -1), size);
    let d = load_src(sp + vec2<i32>(-1, 0), size);
    let e = load_src(sp, size);
    let f = load_src(sp + vec2<i32>(1, 0), size);
    let h = load_src(sp + vec2<i32>(0, 1), size);

    // Luma times 2.
    let b_l = b.b * 0.5 + (b.r * 0.5 + b.g);
    let d_l = d.b * 0.5 + (d.r * 0.5 + d.g);
    let e_l = e.b * 0.5 + (e.r * 0.5 + e.g);
    let f_l = f.b * 0.5 + (f.r * 0.5 + f.g);
    let h_l = h.b * 0.5 + (h.r * 0.5 + h.g);

    // Noise detection.
    var nz = 0.25 * b_l + 0.25 * d_l + 0.25 * f_l + 0.25 * h_l - e_l;
    nz = saturate(abs(nz) * aprx_med_rcp(max3f(max3f(b_l, d_l, e_l), f_l, h_l) - min3f(min3f(b_l, d_l, e_l), f_l, h_l)));
    nz = -0.5 * nz + 1.0;

    // Min and max of ring.
    let mn4 = min(min(b, d), min(f, h));
    let mx4 = max(max(b, d), max(f, h));
    // Immediate constants for peak range.
    let peak_c = vec2<f32>(1.0, -1.0 * 4.0);
    // Limiters, these need to be high precision RCPs.
    let hit_min = mn4 * (vec3<f32>(1.0) / (4.0 * mx4));
    let hit_max = (peak_c.x - mx4) * (vec3<f32>(1.0) / (4.0 * mn4 + peak_c.y));
    let lobe_rgb = max(-hit_min, hit_max);
    var lobe = max(-FSR_RCAS_LIMIT, min(max3f(lobe_rgb.r, lobe_rgb.g, lobe_rgb.b), 0.0)) * c.con.x;

    // Apply noise removal.
    if (c.con.y != 0.0) {
        lobe = lobe * nz;
    }

    // Resolve, which needs the medium precision rcp approximation to avoid visible tonality changes.
    let rcp_l = aprx_med_rcp(4.0 * lobe + 1.0);
    let pix = (lobe * b + lobe * d + lobe * h + lobe * f + e) * rcp_l;
    textureStore(dst, gid.xy, vec4<f32>(pix, 1.0));
}
