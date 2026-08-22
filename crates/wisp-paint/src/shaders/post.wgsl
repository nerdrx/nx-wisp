// Post passes: the separable blur that stands in for `backdrop-filter`, and
// the supersample resolve.
//
// No compute passes exist anywhere in this crate. That is deliberate: SPEC
// §3.1's T3 ("Lobotomised") requires the rig to draw with no compute at all,
// and a renderer that only *sometimes* uses compute would need two code paths
// to prove it.

struct Globals {
    // (logical width, logical height, supersample factor, unused)
    viewport: vec4<f32>,
    // (dir.x, dir.y, radius px, saturate)
    blur: vec4<f32>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var tex: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    // Fullscreen triangle, same trick as the M0 spike.
    var p = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -3.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(3.0, 1.0),
    );
    var o: VsOut;
    o.clip = vec4<f32>(p[i], 0.0, 1.0);
    o.uv = vec2<f32>((p[i].x + 1.0) * 0.5, (1.0 - p[i].y) * 0.5);
    return o;
}

/// Resolve: one bilinear tap per destination pixel. At ss = 2 the tap lands
/// exactly between four source texels, so hardware filtering gives the exact
/// 2×2 box average; at ss = 1 it lands on a texel centre and copies.
@fragment
fn fs_resolve(o: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, o.uv);
}

/// One axis of a separable Gaussian, 13 taps. Operates on premultiplied
/// values, which is exactly where a weighted sum is meaningful.
@fragment
fn fs_blur(o: VsOut) -> @location(0) vec4<f32> {
    let dims = vec2<f32>(textureDimensions(tex, 0));
    let texel = 1.0 / dims;
    let dir = g.blur.xy * texel;
    // sigma ≈ radius / 3 keeps the visible extent at about `radius`.
    let sigma = max(g.blur.z / 3.0, 0.0001);
    let step = max(g.blur.z / 6.0, 1.0);

    var sum = vec4<f32>(0.0);
    var wsum = 0.0;
    for (var i: i32 = -6; i <= 6; i = i + 1) {
        let d = f32(i) * step;
        let w = exp(-0.5 * (d * d) / (sigma * sigma));
        sum = sum + textureSample(tex, samp, o.uv + dir * d) * w;
        wsum = wsum + w;
    }
    var c = sum / max(wsum, 1e-6);

    // `saturate(170%)` from the token, applied on the second axis only (the
    // caller passes 1.0 for the first). Unpremultiply, saturate, repremultiply
    // so the hue shift does not scale with coverage.
    let s = g.blur.w;
    if (s != 1.0 && c.a > 0.001) {
        let straight = c.rgb / c.a;
        let lum = dot(straight, vec3<f32>(0.2126, 0.7152, 0.0722));
        let sat = clamp(vec3<f32>(lum) + (straight - vec3<f32>(lum)) * s, vec3<f32>(0.0), vec3<f32>(1.0));
        c = vec4<f32>(sat * c.a, c.a);
    }
    return c;
}
