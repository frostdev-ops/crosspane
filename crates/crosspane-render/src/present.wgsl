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
    if edge.dimensions.w != 0u && all(pixel < textureDimensions(canvas)) {
        // Integer texel loads are exact nearest sampling at 1:1, with no interpolation.
        return textureLoad(canvas, vec2<i32>(pixel), 0);
    }
    // This value is replaced for an sRGB target at pipeline creation.
    return vec4<f32>(GREY, GREY, GREY, 1.0);
}
