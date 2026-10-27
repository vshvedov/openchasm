// Scene-linear HDR -> display color -> display-space UI -> SDR attachment.
// Independently implemented mathematical behavior, not copied game shaders.
struct Params {
    dims: vec4<u32>,
    viewport: vec4<f32>,
    tone: vec4<f32>,
    flags: vec4<u32>,
}
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> scene: array<f32>;
@group(0) @binding(2) var<storage, read> overlay: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read> bright: array<vec4<f32>>;

@vertex
fn vertex(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let vertices = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    return vec4<f32>(vertices[index], 0.0, 1.0);
}

fn encode_srgb(v: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(v, vec3<f32>(1.0 / 2.4)) - 0.055,
        12.92 * v, v <= vec3<f32>(0.0031308));
}
fn decode_srgb(v: vec3<f32>) -> vec3<f32> {
    return select(pow((v + 0.055) / 1.055, vec3<f32>(2.4)),
        v / 12.92, v <= vec3<f32>(0.04045));
}
fn bloom_at(pixel: vec2<u32>) -> vec3<f32> {
    let sample = max((vec2<f32>(pixel) + 0.5) * 0.5 - 0.5, vec2<f32>(0.0));
    let low = min(vec2<u32>(sample), params.dims.zw - 1u);
    let high = min(low + 1u, params.dims.zw - 1u);
    let f = fract(sample);
    let a = bright[low.y * params.dims.z + low.x].xyz;
    let b = bright[low.y * params.dims.z + high.x].xyz;
    let c = bright[high.y * params.dims.z + low.x].xyz;
    let d = bright[high.y * params.dims.z + high.x].xyz;
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

@fragment
fn fragment(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let relative = position.xy - params.viewport.xy;
    if any(relative < vec2<f32>(0.0)) || any(relative >= params.viewport.zw) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    let pixel = min(vec2<u32>(relative * vec2<f32>(params.dims.xy) / params.viewport.zw),
        params.dims.xy - 1u);
    let index = pixel.y * params.dims.x + pixel.x;
    let i = index * 3u;
    var color = vec3<f32>(scene[i], scene[i + 1u], scene[i + 2u]);
    if params.flags.y == 0u {
        if params.tone.w > 0.0 && params.tone.z > 0.0 {
            color += bloom_at(pixel) * params.tone.w;
        }
        color = min(color, vec3<f32>(1000000.0));
        if params.tone.x != 1.0 || params.tone.y != 1.0 {
            let luminance = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722));
            if luminance > 0.0 {
                // Saturation zero is achromatic, including zero-valued channels.
                var chroma = vec3<f32>(1.0);
                if params.tone.y > 0.0 {
                    chroma = pow(color / luminance, vec3<f32>(params.tone.y));
                }
                // Avoid 0 * infinity at extreme valid gamma settings. The CPU
                // maps that NaN to black and positive overflow to display white.
                let power = log2(luminance) / params.tone.x;
                let overflow = power >= 128.0;
                let raised = exp2(min(power, 127.99998));
                for (var channel = 0u; channel < 3u; channel++) {
                    if chroma[channel] == 0.0 { color[channel] = 0.0; }
                    else if overflow { color[channel] = 3.0e38; }
                    else {
                        color[channel] = min(chroma[channel], 3.0e38 / max(raised, 1.0)) * raised;
                    }
                }
            } else { color = vec3<f32>(0.0); }
        }
        // Clamp before multiplying: gamma may legitimately exceed f32 range.
        // Exposure zero must produce black rather than infinity * zero.
        if params.tone.z == 0.0 { color = vec3<f32>(0.0); }
        else { color = min(color, vec3<f32>(1000000.0 / params.tone.z)) * params.tone.z; }
        if params.flags.x != 0u {
            color = color * (2.51 * color + 0.03) / (color * (2.43 * color + 0.59) + 0.14);
        }
        color = encode_srgb(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)));
    }
    if params.flags.z != 0u {
        let ui = overlay[index];
        color = color * (1.0 - ui.w) + ui.xyz;
    }
    color = clamp(color, vec3<f32>(0.0), vec3<f32>(1.0));
    if params.flags.w != 0u { color = decode_srgb(color); }
    return vec4<f32>(color, 1.0);
}
