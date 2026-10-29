// NV12 -> RGBA8 colour conversion.
//
// WGSL port of the `convertVideo` Metal kernel embedded in qt/src/app/MetalVideoItem.mm.
// The Y plane is an R8Unorm texture, the interleaved Cb/Cr plane an Rg8Unorm texture at
// half resolution that is sampled bilinearly (clamp to edge), exactly like the Metal
// `texture2d<float, access::sample>` with `filter::linear`.
//
// `params` is the `float coefficients[8]` the Qt kernel receives in `buffer(0)` as two
// float4s (see `xcloud_media::ColorInfo::shader_coefficients`):
//   p0 = (y_offset, y_scale, c_scale, 0)   limited range: 16/255, 255/219, 255/224
//                                           full range:    0,      1,       1
//   p1 = (r_cr, g_cb, g_cr, b_cb)           BT.709: 1.5748, -0.187324, -0.468124, 1.8556
//                                           BT.601: 1.402,  -0.344136, -0.714136, 1.772
//                                           BT.2020: 1.4746, -0.164553, -0.571353, 1.8814

struct ConvertParams {
    p0: vec4<f32>,
    p1: vec4<f32>,
};

@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var uv_plane: texture_2d<f32>;
@group(0) @binding(2) var uv_sampler: sampler;
@group(0) @binding(3) var rgb_out: texture_storage_2d<rgba8unorm, write>;
@group(0) @binding(4) var<uniform> params: ConvertParams;

@compute @workgroup_size(16, 16, 1)
fn convert_video(@builtin(global_invocation_id) gid: vec3<u32>) {
    let xy = gid.xy;
    let size = textureDimensions(rgb_out);
    if (xy.x >= size.x || xy.y >= size.y) {
        return;
    }
    let l = (textureLoad(y_plane, xy, 0).r - params.p0.x) * params.p0.y;
    let uv_coord = (vec2<f32>(xy) + vec2<f32>(0.5, 0.5)) / vec2<f32>(size);
    let c = (textureSampleLevel(uv_plane, uv_sampler, uv_coord, 0.0).rg - vec2<f32>(0.5, 0.5)) * params.p0.z;
    let color = vec3<f32>(
        l + params.p1.x * c.y,
        l + params.p1.y * c.x + params.p1.z * c.y,
        l + params.p1.w * c.x,
    );
    textureStore(rgb_out, xy, vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0));
}
