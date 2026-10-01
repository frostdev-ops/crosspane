struct Params {
 origin: vec2<u32>, size: vec2<u32>, coded: vec2<u32>, offsets: vec2<u32>,
 pitches: vec2<u32>, pad: vec2<u32>, r0: vec4<f32>, r1: vec4<f32>, r2: vec4<f32>
}
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;
fn rgb(x: u32,y: u32)->vec3<f32> {
 return textureLoad(frame,vec2<i32>(p.origin+min(vec2(x,y),p.size-vec2(1u))),0).rgb;
}
fn sample_code(v: vec3<f32>, row: vec4<f32>)->u32 {
 return u32(clamp(floor((dot(v,row.xyz)+row.w)*255.0+0.5),0.0,255.0));
}
fn chroma(x:u32,y:u32)->vec2<u32> {
 let v=(rgb(x,y)+rgb(x+1u,y)+rgb(x,y+1u)+rgb(x+1u,y+1u))*0.25;
 return vec2(sample_code(v,p.r1),sample_code(v,p.r2));
}
@group(0) @binding(2) var<storage,read_write> output: array<u32>;
@compute @workgroup_size(8,8)
fn main(@builtin(global_invocation_id) id:vec3<u32>) {
 let x=id.x*4u; let y=id.y;
 if x>=p.coded.x || y>=p.coded.y { return; }
 var word=0u;
 for(var j=0u;j<4u;j++) {
  if x+j<p.coded.x { word |= sample_code(rgb(x+j,y),p.r0)<<(j*8u); }
 }
 let yi=p.offsets.x+y*(p.pitches.x/4u)+x/4u;
 // Preserve padding bytes of a last half-word.
 if x+2u==p.coded.x { word |= output[yi]&0xffff0000u; }
 output[yi]=word;
 if y<p.coded.y/2u {
  word=0u;
  for(var j=0u;j<4u;j+=2u) {
   if x+j<p.coded.x { let uv=chroma(x+j,y*2u); word |= (uv.x | (uv.y<<8u))<<(j*8u); }
  }
  let ui=p.offsets.y+y*(p.pitches.y/4u)+x/4u;
  if x+2u==p.coded.x { word |= output[ui]&0xffff0000u; }
  output[ui]=word;
 }
}
