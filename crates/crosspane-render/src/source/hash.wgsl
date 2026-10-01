struct Params { origin: vec2<u32>, size: vec2<u32>, grid: vec2<u32>, full: u32, pad: u32 }
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var<uniform> p: Params;
@group(0) @binding(2) var<storage, read> old: array<vec2<u32>>;
@group(0) @binding(3) var<storage, read_write> hashes: array<vec2<u32>>;
@group(0) @binding(4) var<storage, read_write> bits: array<atomic<u32>>;
var<workgroup> columns: array<vec2<u32>, 64>;
fn rot(v: vec2<u32>, n: u32) -> vec2<u32> { return (v << vec2(n)) | (v >> vec2(32u-n)); }
@compute @workgroup_size(64)
fn main(@builtin(workgroup_id) group: vec3<u32>, @builtin(local_invocation_index) t: u32) {
 let base = group.xy * 64u;
 let dim = min(vec2(64u), p.size-base);
 let p1 = vec2(2654435761u, 2246822519u);
 let p2 = vec2(2246822519u, 3266489917u);
 let p3 = vec2(3266489917u, 668265263u);
 var h = vec2(374761393u, 1315423911u) ^ (dim.x*p1 + dim.y*p2) ^ (t*p3);
 if t < dim.x {
  for (var row=0u; row<dim.y; row++) {
   let v = pack4x8unorm(textureLoad(frame, vec2<i32>(p.origin+base+vec2(t,row)), 0).bgra);
   h = rot(h + ((vec2(v) ^ (row*p3))*p2), 13u)*p1;
  }
 }
 columns[t] = h;
 workgroupBarrier();
 for (var step=32u; step>0u; step=step/2u) {
  if t<step { columns[t] = columns[t]*p1 + rot(columns[t+step] ^ ((t+step)*p3),17u); }
  workgroupBarrier();
 }
 if t==0u {
  h=columns[0]; h=h^(h>>vec2(15u)); h=h*p2; h=h^(h>>vec2(13u)); h=h*p3; h=h^(h>>vec2(16u));
  let i=group.y*p.grid.x+group.x;
  hashes[i]=h;
  if p.full!=0u || any(h!=old[i]) { atomicOr(&bits[i/32u],1u<<(i%32u)); }
 }
}
