// Independently implemented enhancement bloom; original appearance disables it.
struct Params {
    dims: vec4<u32>,
    viewport: vec4<f32>,
    tone: vec4<f32>,
    flags: vec4<u32>,
}
@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> scene: array<f32>;
@group(0) @binding(2) var<storage, read_write> bright: array<vec4<f32>>;
@group(0) @binding(3) var<storage, read_write> scratch: array<vec4<f32>>;

fn source(x: u32, y: u32) -> vec3<f32> {
    let i = (y * params.dims.x + x) * 3u;
    return vec3<f32>(scene[i], scene[i + 1u], scene[i + 2u]);
}

@compute @workgroup_size(8, 8)
fn extract(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= params.dims.zw) { return; }
    let start = id.xy * 2u;
    let end = min(start + vec2<u32>(2u), params.dims.xy);
    var sum = vec3<f32>(0.0);
    for (var y = start.y; y < end.y; y++) {
        for (var x = start.x; x < end.x; x++) {
            let color = source(x, y);
            let exposed = dot(color, vec3<f32>(0.2126, 0.7152, 0.0722)) * params.tone.z;
            if exposed > 1.0 { sum += color * (1.0 - 1.0 / exposed); }
        }
    }
    let count = (end.x - start.x) * (end.y - start.y);
    bright[id.y * params.dims.z + id.x] = vec4<f32>(sum / f32(count), 0.0);
}

const WEIGHTS = array<f32, 5>(0.0625, 0.25, 0.375, 0.25, 0.0625);

@compute @workgroup_size(8, 8)
fn horizontal(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= params.dims.zw) { return; }
    var sum = vec3<f32>(0.0);
    for (var tap = 0; tap < 5; tap++) {
        let x = u32(clamp(i32(id.x) + tap - 2, 0, i32(params.dims.z) - 1));
        sum += bright[id.y * params.dims.z + x].xyz * WEIGHTS[tap];
    }
    scratch[id.y * params.dims.z + id.x] = vec4<f32>(sum, 0.0);
}

@compute @workgroup_size(8, 8)
fn vertical(@builtin(global_invocation_id) id: vec3<u32>) {
    if any(id.xy >= params.dims.zw) { return; }
    var sum = vec3<f32>(0.0);
    for (var tap = 0; tap < 5; tap++) {
        let y = u32(clamp(i32(id.y) + tap - 2, 0, i32(params.dims.w) - 1));
        sum += scratch[y * params.dims.z + id.x].xyz * WEIGHTS[tap];
    }
    bright[id.y * params.dims.z + id.x] = vec4<f32>(sum, 0.0);
}
