struct Params { origin: vec2<u32>, size: vec2<u32>, grid: vec2<u32>, full: u32, pad: u32 }
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;
@group(0) @binding(2) var<storage, read> slots: array<u32>;
@group(0) @binding(3) var<storage, read_write> pixels: array<u32>;
@compute @workgroup_size(64)
fn main(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) t: u32) {
 let slot=slots[group.y*p.grid.x+group.x];
 if slot==0xffffffffu { return; }
 let base=group.xy*64u;
 let dim=min(vec2(64u),p.size-base);
 for(var i=t; i<dim.x*dim.y; i+=64u) {
  let xy=base+vec2(i%dim.x,i/dim.x);
  pixels[slot*4096u+i]=pack4x8unorm(textureLoad(frame,vec2<i32>(p.origin+xy),0).bgra);
 }
}
