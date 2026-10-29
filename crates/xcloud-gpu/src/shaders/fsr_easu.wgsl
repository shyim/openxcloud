// AMD FidelityFX Super Resolution 1.0 - EASU (Edge Adaptive Spatial Upsampling)
//
// WGSL port of `FsrEasuF` from ffx_fsr1.h (FidelityFX-FSR, AMD, 2021).
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
// Differences from the reference: the four `textureGather` calls are replaced by twelve
// clamped `textureLoad`s of the same texels (identical values with a clamp-to-edge
// sampler), which is why only `con0` is read here; `con1..con3` are kept so the uniform
// is exactly what `FsrEasuCon` produces.
//
// 12-tap pattern around the source position `fp` (the texel containing the output sample):
//
//      +---+---+
//      | b | c |
//  +---+---+---+---+
//  | e | f | g | h |
//  +---+---+---+---+
//  | i | j | k | l |
//  +---+---+---+---+
//      | n | o |
//      +---+---+

struct EasuParams {
    // (in_w / out_w, in_h / out_h, 0.5 * in_w / out_w - 0.5, 0.5 * in_h / out_h - 0.5)
    con0: vec4<f32>,
    // (1 / in_w, 1 / in_h, 1 / in_w, -1 / in_h)
    con1: vec4<f32>,
    // (-1 / in_w, 2 / in_h, 1 / in_w, 2 / in_h)
    con2: vec4<f32>,
    // (0, 4 / in_h, 0, 0)
    con3: vec4<f32>,
};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(2) var<uniform> c: EasuParams;

// ---- ffx_a.h approximations --------------------------------------------------------

// APrxLoRcpF1
fn aprx_lo_rcp(a: f32) -> f32 {
    return bitcast<f32>(0x7ef07ebbu - bitcast<u32>(a));
}

// APrxLoRsqF1
fn aprx_lo_rsq(a: f32) -> f32 {
    return bitcast<f32>(0x5f347d74u - (bitcast<u32>(a) >> 1u));
}

fn min3(x: vec3<f32>, y: vec3<f32>, z: vec3<f32>) -> vec3<f32> {
    return min(x, min(y, z));
}

fn max3(x: vec3<f32>, y: vec3<f32>, z: vec3<f32>) -> vec3<f32> {
    return max(x, max(y, z));
}

fn load_src(p: vec2<i32>, size: vec2<i32>) -> vec3<f32> {
    let q = clamp(p, vec2<i32>(0, 0), size - vec2<i32>(1, 1));
    return textureLoad(src, q, 0).rgb;
}

// Simplest multi-channel approximate luma possible (luma times 2, in 2 FMA/MAD).
fn luma2(col: vec3<f32>) -> f32 {
    return col.b * 0.5 + (col.r * 0.5 + col.g);
}

// FsrEasuTapF: filtering for a given tap for the scalar.
fn easu_tap(
    ac: ptr<function, vec3<f32>>,
    aw: ptr<function, f32>,
    off: vec2<f32>,
    dir: vec2<f32>,
    len: vec2<f32>,
    lob: f32,
    clp: f32,
    col: vec3<f32>,
) {
    // Rotate offset by direction.
    var v: vec2<f32>;
    v.x = (off.x * dir.x) + (off.y * dir.y);
    v.y = (off.x * (-dir.y)) + (off.y * dir.x);
    // Anisotropy.
    v = v * len;
    // Compute distance^2.
    var d2 = v.x * v.x + v.y * v.y;
    // Limit to the window as at corner, 2 taps can easily be outside.
    d2 = min(d2, clp);
    // Approximation of lancos2 without sin() or rcp(), or sqrt() to get x.
    //  (25/16 * (2/5 * x^2 - 1)^2 - (25/16 - 1)) * (1/4 * x^2 - 1)^2
    //  |_______________________________________|   |_______________|
    //                   base                             window
    // The general form of the 'base' is,
    //  (a*(b*x^2-1)^2-(a-1))
    // Where 'a=1/(2*b-b^2)' and 'b' moves around the negative lobe.
    var wb = (2.0 / 5.0) * d2 - 1.0;
    var wa = lob * d2 - 1.0;
    wb = wb * wb;
    wa = wa * wa;
    wb = (25.0 / 16.0) * wb - (25.0 / 16.0 - 1.0);
    let w = wb * wa;
    // Do weighted average.
    *ac = *ac + col * w;
    *aw = *aw + w;
}

// FsrEasuSetF: accumulate direction and length.
fn easu_set(
    dir: ptr<function, vec2<f32>>,
    len: ptr<function, f32>,
    pp: vec2<f32>,
    bi_s: bool,
    bi_t: bool,
    bi_u: bool,
    bi_v: bool,
    la: f32,
    lb: f32,
    lc: f32,
    ld: f32,
    le: f32,
) {
    // Compute bilinear weight, branches factor out as predicates are compiler time immediates.
    //  s t
    //  u v
    var w = 0.0;
    if (bi_s) {
        w = (1.0 - pp.x) * (1.0 - pp.y);
    }
    if (bi_t) {
        w = pp.x * (1.0 - pp.y);
    }
    if (bi_u) {
        w = (1.0 - pp.x) * pp.y;
    }
    if (bi_v) {
        w = pp.x * pp.y;
    }
    // Direction is the '+' diff.
    //    a
    //  b c d
    //    e
    // Then takes magnitude from abs average of both sides of 'c'.
    // Length converts gradient reversal to 0, smoothly to non-reversal at 1, shaped, then adding horz and vert terms.
    let dc = ld - lc;
    let cb = lc - lb;
    var len_x = max(abs(dc), abs(cb));
    len_x = aprx_lo_rcp(len_x);
    let dir_x = ld - lb;
    var d = *dir;
    d.x = d.x + dir_x * w;
    len_x = saturate(abs(dir_x) * len_x);
    len_x = len_x * len_x;
    var l = *len;
    l = l + len_x * w;
    // Repeat for the y axis.
    let ec = le - lc;
    let ca = lc - la;
    var len_y = max(abs(ec), abs(ca));
    len_y = aprx_lo_rcp(len_y);
    let dir_y = le - la;
    d.y = d.y + dir_y * w;
    len_y = saturate(abs(dir_y) * len_y);
    len_y = len_y * len_y;
    l = l + len_y * w;
    *dir = d;
    *len = l;
}

// FsrEasuF
@compute @workgroup_size(16, 16, 1)
fn easu(@builtin(global_invocation_id) gid: vec3<u32>) {
    let out_size = textureDimensions(dst);
    if (gid.x >= out_size.x || gid.y >= out_size.y) {
        return;
    }
    let in_size = vec2<i32>(textureDimensions(src));

    // Get position of 'f'.
    var pp = vec2<f32>(gid.xy) * c.con0.xy + c.con0.zw;
    let fp = floor(pp);
    pp = pp - fp;
    let base = vec2<i32>(fp);

    // 12-tap kernel.
    let b_c = load_src(base + vec2<i32>(0, -1), in_size);
    let c_c = load_src(base + vec2<i32>(1, -1), in_size);
    let e_c = load_src(base + vec2<i32>(-1, 0), in_size);
    let f_c = load_src(base + vec2<i32>(0, 0), in_size);
    let g_c = load_src(base + vec2<i32>(1, 0), in_size);
    let h_c = load_src(base + vec2<i32>(2, 0), in_size);
    let i_c = load_src(base + vec2<i32>(-1, 1), in_size);
    let j_c = load_src(base + vec2<i32>(0, 1), in_size);
    let k_c = load_src(base + vec2<i32>(1, 1), in_size);
    let l_c = load_src(base + vec2<i32>(2, 1), in_size);
    let n_c = load_src(base + vec2<i32>(0, 2), in_size);
    let o_c = load_src(base + vec2<i32>(1, 2), in_size);

    // Rename.
    let b_l = luma2(b_c);
    let c_l = luma2(c_c);
    let i_l = luma2(i_c);
    let j_l = luma2(j_c);
    let f_l = luma2(f_c);
    let e_l = luma2(e_c);
    let k_l = luma2(k_c);
    let l_l = luma2(l_c);
    let h_l = luma2(h_c);
    let g_l = luma2(g_c);
    let o_l = luma2(o_c);
    let n_l = luma2(n_c);

    // Accumulate for bilinear interpolation.
    var dir = vec2<f32>(0.0, 0.0);
    var len = 0.0;
    easu_set(&dir, &len, pp, true, false, false, false, b_l, e_l, f_l, g_l, j_l);
    easu_set(&dir, &len, pp, false, true, false, false, c_l, f_l, g_l, h_l, k_l);
    easu_set(&dir, &len, pp, false, false, true, false, f_l, i_l, j_l, k_l, n_l);
    easu_set(&dir, &len, pp, false, false, false, true, g_l, j_l, k_l, l_l, o_l);

    // Normalize with approximation, and cleanup close to zero.
    let dir2 = dir * dir;
    var dir_r = dir2.x + dir2.y;
    let zro = dir_r < (1.0 / 32768.0);
    dir_r = aprx_lo_rsq(dir_r);
    dir_r = select(dir_r, 1.0, zro);
    dir.x = select(dir.x, 1.0, zro);
    dir = dir * vec2<f32>(dir_r, dir_r);

    // Transform from {0 to 2} to {0 to 1} range, and shape with square.
    len = len * 0.5;
    len = len * len;
    // Stretch kernel {1.0 vert|horz, to sqrt(2.0) on diagonal}.
    let stretch = (dir.x * dir.x + dir.y * dir.y) * aprx_lo_rcp(max(abs(dir.x), abs(dir.y)));
    // Anisotropic length after rotation,
    //  x := 1.0 lerp to 'stretch' on edges
    //  y := 1.0 lerp to 2x on edges
    let len2 = vec2<f32>(1.0 + (stretch - 1.0) * len, 1.0 + -0.5 * len);
    // Based on the amount of 'edge',
    // the window shifts from +/-{sqrt(2.0) to slightly beyond 2.0}.
    let lob = 0.5 + ((1.0 / 4.0 - 0.04) - 0.5) * len;
    // Set distance^2 clipping point to the end of the adjustable window.
    let clp = aprx_lo_rcp(lob);

    // Accumulation mixed with min/max of 4 nearest.
    //    b c
    //  e f g h
    //  i j k l
    //    n o
    let min4 = min(min3(f_c, g_c, j_c), k_c);
    let max4 = max(max3(f_c, g_c, j_c), k_c);
    // Accumulation.
    var ac = vec3<f32>(0.0, 0.0, 0.0);
    var aw = 0.0;
    easu_tap(&ac, &aw, vec2<f32>(0.0, -1.0) - pp, dir, len2, lob, clp, b_c);
    easu_tap(&ac, &aw, vec2<f32>(1.0, -1.0) - pp, dir, len2, lob, clp, c_c);
    easu_tap(&ac, &aw, vec2<f32>(-1.0, 1.0) - pp, dir, len2, lob, clp, i_c);
    easu_tap(&ac, &aw, vec2<f32>(0.0, 1.0) - pp, dir, len2, lob, clp, j_c);
    easu_tap(&ac, &aw, vec2<f32>(0.0, 0.0) - pp, dir, len2, lob, clp, f_c);
    easu_tap(&ac, &aw, vec2<f32>(-1.0, 0.0) - pp, dir, len2, lob, clp, e_c);
    easu_tap(&ac, &aw, vec2<f32>(1.0, 1.0) - pp, dir, len2, lob, clp, k_c);
    easu_tap(&ac, &aw, vec2<f32>(2.0, 1.0) - pp, dir, len2, lob, clp, l_c);
    easu_tap(&ac, &aw, vec2<f32>(2.0, 0.0) - pp, dir, len2, lob, clp, h_c);
    easu_tap(&ac, &aw, vec2<f32>(1.0, 0.0) - pp, dir, len2, lob, clp, g_c);
    easu_tap(&ac, &aw, vec2<f32>(1.0, 2.0) - pp, dir, len2, lob, clp, o_c);
    easu_tap(&ac, &aw, vec2<f32>(0.0, 2.0) - pp, dir, len2, lob, clp, n_c);

    // Normalize and dering.
    let pix = min(max4, max(min4, ac * vec3<f32>(1.0 / aw)));
    textureStore(dst, gid.xy, vec4<f32>(pix, 1.0));
}
