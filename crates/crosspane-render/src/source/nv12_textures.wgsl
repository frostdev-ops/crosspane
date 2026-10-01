@group(0) @binding(2) var luma: texture_storage_2d<r8unorm,write>;
@group(0) @binding(3) var uv_plane: texture_storage_2d<rg8unorm,write>;
@compute @workgroup_size(8,8)
fn main(@builtin(global_invocation_id) id:vec3<u32>) {
 let x=id.x; let y=id.y;
 if x>=p.coded.x || y>=p.coded.y { return; }
 textureStore(luma,vec2<i32>(id.xy),vec4(f32(sample_code(rgb(x,y),p.r0))/255.0,0.0,0.0,1.0));
 if x%2u==0u && y%2u==0u {
  let uv=chroma(x,y);
  textureStore(uv_plane,vec2<i32>(id.xy/2u),vec4(vec2<f32>(uv)/255.0,0.0,1.0));
 }
}
