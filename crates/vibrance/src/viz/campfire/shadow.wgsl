// Campfire: the shadow map, one face at a time around the fire (depth only).

struct Face {
    view_proj: mat4x4<f32>,
}

struct Draw {
    model: mat4x4<f32>,
    params: vec4<f32>,
}

@group(0) @binding(0) var<uniform> face: Face;
@group(1) @binding(0) var<uniform> draw: Draw;

@vertex
fn vs_shadow(@location(0) pos: vec3<f32>) -> @builtin(position) vec4<f32> {
    return face.view_proj * draw.model * vec4(pos, 1.0);
}
