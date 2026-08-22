// Textured quads: glyph coverage, atlas sprites, and the blurred backdrop
// drawn back through a rounded-rect mask.

struct Globals {
    viewport: vec4<f32>,
    blur: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
    // Premultiplied sRGB.
    @location(2) tint: vec4<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) tint: vec4<f32>,
};

@vertex
fn vs(i: VsIn) -> VsOut {
    var o: VsOut;
    let ndc = vec2<f32>(
        i.pos.x / g.viewport.x * 2.0 - 1.0,
        1.0 - i.pos.y / g.viewport.y * 2.0,
    );
    o.clip = vec4<f32>(ndc, 0.0, 1.0);
    o.uv = i.uv;
    o.tint = i.tint;
    return o;
}

/// RGBA source, already premultiplied (atlas sprites, the blurred backdrop).
@fragment
fn fs_rgba(o: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, o.uv) * o.tint;
}

/// Single-channel coverage (glyph rasters). The tint carries the colour.
@fragment
fn fs_alpha(o: VsOut) -> @location(0) vec4<f32> {
    let cov = textureSample(tex, samp, o.uv).r;
    return o.tint * cov;
}
