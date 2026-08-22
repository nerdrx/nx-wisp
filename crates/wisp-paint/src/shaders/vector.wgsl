// The one vector pipeline: tessellated geometry, per-shape paint records,
// premultiplied sRGB out.
//
// Colours are premultiplied *sRGB*, not linear. The render targets are
// Rgba8Unorm (never -Srgb), so what the shader writes is the 8-bit code, and
// blending happens exactly where CSS does it — which is where DESIGN.md's
// tokens were tuned.

struct Globals {
    // (logical width, logical height, supersample factor, unused)
    viewport: vec4<f32>,
    // (dir.x, dir.y, radius px, saturate) — only the blur pass reads this.
    blur: vec4<f32>,
};

struct Paint {
    kind: u32,
    n_stops: u32,
    pad0: u32,
    pad1: u32,
    // Linear: (dx, dy, bbox_w, bbox_h). Radial: (cx, cy, rx, ry).
    params: vec4<f32>,
    offsets: array<vec4<f32>, 2>,
    colors: array<vec4<f32>, 6>,
};

@group(0) @binding(0) var<uniform> g: Globals;
@group(1) @binding(0) var<storage, read> paints: array<Paint>;

struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) paint: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) @interpolate(flat) paint: u32,
};

@vertex
fn vs(i: VsIn) -> VsOut {
    var o: VsOut;
    let ndc = vec2<f32>(
        i.pos.x / g.viewport.x * 2.0 - 1.0,
        1.0 - i.pos.y / g.viewport.y * 2.0,
    );
    o.clip = vec4<f32>(ndc, 0.0, 1.0);
    o.local = i.local;
    o.paint = i.paint;
    return o;
}

fn offset_at(idx: u32, i: u32) -> f32 {
    return paints[idx].offsets[i / 4u][i % 4u];
}

@fragment
fn fs(o: VsOut) -> @location(0) vec4<f32> {
    let idx = o.paint;
    let kind = paints[idx].kind;

    var t = 0.0;
    if (kind == 1u) {
        // CSS linear-gradient: the gradient line through the box centre, with
        // length |w·sin a| + |h·cos a|.
        let d = paints[idx].params.xy;
        let sz = paints[idx].params.zw;
        let p = vec2<f32>(o.local.x * sz.x, o.local.y * sz.y) - sz * 0.5;
        let len = abs(sz.x * d.x) + abs(sz.y * d.y);
        t = dot(p, d) / max(len, 1e-6) + 0.5;
    } else if (kind == 2u) {
        let c = paints[idx].params.xy;
        let r = paints[idx].params.zw;
        t = length((o.local - c) / r);
    }

    let n = paints[idx].n_stops;
    if (n <= 1u) {
        return paints[idx].colors[0];
    }

    let tc = clamp(t, 0.0, 1.0);
    if (tc <= offset_at(idx, 0u)) {
        return paints[idx].colors[0];
    }
    var col = paints[idx].colors[n - 1u];
    for (var i: u32 = 0u; i + 1u < n; i = i + 1u) {
        let a = offset_at(idx, i);
        let b = offset_at(idx, i + 1u);
        if (tc >= a && tc <= b) {
            let span = max(b - a, 1e-6);
            col = mix(paints[idx].colors[i], paints[idx].colors[i + 1u], (tc - a) / span);
            break;
        }
    }
    return col;
}
