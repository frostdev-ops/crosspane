struct Target {
    size: vec2<u32>,
}

@group(0) @binding(0) var<uniform> image: Target;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let x = u32(position.x);
    let y = u32(position.y);
    let w = image.size.x;
    let h = image.size.y;
    var rgb: vec3<u32>;
    if x == 0u || y == 0u || x == w - 1u || y == h - 1u {
        rgb = vec3<u32>(255u, 0u, 0u);
    } else if x < 256u && y < 256u {
        rgb = vec3<u32>(select(0u, 255u, (x + y) % 2u == 0u));
    } else if y >= h - min(h, 32u) {
        var v = 0u;
        if w > 1u {
            v = x * 255u / (w - 1u);
        }
        rgb = vec3<u32>(v);
    } else {
        let bars = array<vec3<u32>, 8>(
            vec3<u32>(255u, 255u, 255u),
            vec3<u32>(255u, 255u, 0u),
            vec3<u32>(0u, 255u, 255u),
            vec3<u32>(0u, 255u, 0u),
            vec3<u32>(255u, 0u, 255u),
            vec3<u32>(255u, 0u, 0u),
            vec3<u32>(0u, 0u, 255u),
            vec3<u32>(0u, 0u, 0u),
        );
        rgb = bars[x * 8u / w];
    }
    return vec4<f32>(vec3<f32>(rgb) / 255.0, 1.0);
}
