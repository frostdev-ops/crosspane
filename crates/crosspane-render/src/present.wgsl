@group(1) @binding(0) var luma: texture_2d<f32>;
@group(1) @binding(1) var chroma: texture_2d<f32>;
@group(2) @binding(0) var sources: texture_2d<u32>;
struct Conversion {
    offset: vec4<f32>,
    red: vec4<f32>,
    green: vec4<f32>,
    blue: vec4<f32>,
    visible: vec4<u32>,
}
@group(1) @binding(2) var<uniform> conversion: Conversion;

fn srgb_decode(rgb: vec3<f32>) -> vec3<f32> {
    return select(
        pow((rgb + vec3(0.055)) / 1.055, vec3(2.4)),
        rgb / 12.92,
        rgb <= vec3(0.04045),
    );
}

@group(0) @binding(0) var canvas: texture_2d<f32>;

struct Edge {
    colour: vec4<f32>,
    dimensions: vec4<u32>,
}
@group(0) @binding(1) var<uniform> edge: Edge;

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(vec2(-1.0, -1.0), vec2(3.0, -1.0), vec2(-1.0, 3.0));
    return vec4(positions[index], 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let pixel = vec2<u32>(position.xy);
    let size = edge.dimensions.xy;
    let width = edge.dimensions.z;
    if any(pixel < vec2<u32>(width)) || any(size - pixel <= vec2<u32>(width)) {
        return edge.colour;
    }
    let tile = pixel / 64u;
    if (edge.dimensions.w & 2u) != 0u && all(tile < textureDimensions(sources)) &&
        all(pixel >= conversion.visible.xy) && all(pixel < conversion.visible.zw) {
        if textureLoad(sources, vec2<i32>(tile), 0).r != 0u {
            let local = pixel - conversion.visible.xy;
            let y = textureLoad(luma, vec2<i32>(local), 0).r;
            // Match the reference's shared chroma pair for each 2×2 luma block.
            let uv = textureLoad(chroma, vec2<i32>(local / 2u), 0).rg;
            let sample = vec3(y, uv) - conversion.offset.xyz;
            var rgb = clamp(vec3(
                dot(conversion.red.xyz, sample),
                dot(conversion.green.xyz, sample),
                dot(conversion.blue.xyz, sample),
            ), vec3(0.0), vec3(1.0));
            // Undo the target's sRGB encoding so its stored bytes match the reference.
            if SRGB {
                rgb = srgb_decode(rgb);
            }
            return vec4(rgb, 1.0);
        }
    }
    if (edge.dimensions.w & 1u) != 0u && all(pixel < textureDimensions(canvas)) {
        // Integer texel loads are exact nearest sampling at 1:1, with no interpolation.
        return textureLoad(canvas, vec2<i32>(pixel), 0);
    }
    // This value is replaced for an sRGB target at pipeline creation.
    return vec4<f32>(GREY, GREY, GREY, 1.0);
}
