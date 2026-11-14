// Screen-space contact shadows over the engine's own linear depth (see docs/contact-shadows.md).
//
// Two full-screen draws (one triangle each, no input layout), both at HALF resolution of the scene:
//   ps_contact  depth -> raw visibility (R16_FLOAT, 1 = lit): a short ray march towards the light, steps jittered by
//               interleaved gradient noise (a fixed screen-space pattern)
//   ps_blur     4x4 depth-aware blur of the raw visibility (kills the noise pattern without crossing depth edges)
// The light-adjust substitute (light_adjust_hdr.hlsl) upsamples the result depth-aware and applies it softly.
//
// Only view-space geometry from the depth is used. The world length of the ray is given in depth units (the
// CPU converts 0.35 m with depth_unit, metres per 1.0 of the normalised depth) and clamped to a fraction of the pixel
// depth, so a wrong unit can only make the shadow too short or too long on screen within sane bounds.
//
// c0: xyz = direction TO the light in view space (normalized, +z away from the camera), w = fade (0..1: confidence
//     of the direction; the visibility is 1 - fade * shadow)
// c1: x = 1/width, y = 1/height, z = width, w = height (FULL scene size)
// c2: x = ray length in depth units, y = min length as a fraction of the pixel depth, z = max length (fraction),
//     w = thickness of an occluder in ray lengths
// c3: x = tan(vertical half fov), y = width / height, z = steps, w = depth bias as a fraction of the pixel depth
// t10 = linear depth (R32_FLOAT), t11 = raw visibility (R16_FLOAT, blur only)

cbuffer params : register(b0)
{
    float4 c0;
    float4 c1;
    float4 c2;
    float4 c3;
};

Texture2D<float> depth : register(t10);
Texture2D<float> vis_in : register(t11);

struct VSOut
{
    float4 pos : SV_POSITION;
};

VSOut vs_main(uint id : SV_VertexID)
{
    float2 p = float2((id << 1) & 2, id & 2);
    VSOut o;
    o.pos = float4(p * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

// The engine's depth is NORMALISED (1.0 = far plane, written for the sky; metres = z * depth_unit, see aerial.rs): the far
// plane (>= 0.98) is the sky, neither a surface to shadow nor an occluder.
bool valid(float z)
{
    return z > 1e-5 && isfinite(z) && z < 0.98;
}

// Depth at a full-size pixel; outside the image it is 0 = invalid (never an occluder or a neighbour).
float D(int2 p)
{
    if (any(p < int2(0, 0)) || any(p >= int2(c1.zw)))
        return 0.0;
    return depth.Load(int3(p, 0));
}

float3 VP(int2 p, float z)
{
    float2 uv = (float2(p) + 0.5) * c1.xy;
    float2 ndc = float2(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
    return float3(ndc.x * c3.y * c3.x * z, ndc.y * c3.x * z, z);
}

float ign(float2 p)
{
    return frac(52.9829189 * frac(dot(p, float2(0.06711056, 0.00583715))));
}

// The full-size pixel a half-size pixel reads its depth from (the same one in both passes and in the upsample).
int2 full_of(int2 hp)
{
    return min(hp * 2, int2(c1.zw) - 1);
}

float ps_contact(float4 sv : SV_POSITION) : SV_Target
{
    int2 hp = int2(sv.xy);
    int2 p = full_of(hp);
    float z = D(p);
    if (!valid(z) || c0.w <= 0.0)
        return 1.0;
    float3 P = VP(p, z);

    // Normal from the smaller of the two one-sided depth differences per axis (as the AO does).
    float zl = D(p + int2(-1, 0));
    float zr = D(p + int2(1, 0));
    float zu = D(p + int2(0, -1));
    float zd = D(p + int2(0, 1));
    float dl = valid(zl) ? abs(z - zl) : 1e30;
    float dr = valid(zr) ? abs(zr - z) : 1e30;
    float du = valid(zu) ? abs(z - zu) : 1e30;
    float dd = valid(zd) ? abs(zd - z) : 1e30;
    if (min(min(dl, dr), min(du, dd)) > 1e29)
        return 1.0; // an isolated pixel has no surface to shadow
    float3 dx = (dr < dl) ? (VP(p + int2(1, 0), zr) - P) : (P - VP(p + int2(-1, 0), zl));
    float3 dy = (dd < du) ? (VP(p + int2(0, 1), zd) - P) : (P - VP(p + int2(0, -1), zu));
    float3 n = cross(dx, dy);
    float nlen = length(n);
    if (nlen < 1e-20)
        return 1.0;
    float3 N = n / nlen;
    if (dot(N, P) > 0.0)
        N = -N;

    // A surface that faces away from the light is in its own shade already: nothing to cast onto it.
    float3 L = c0.xyz;
    float nl = dot(N, L);
    float gate = saturate((nl - 0.08) / 0.25);
    if (gate <= 0.0)
        return 1.0;

    float len = clamp(c2.x, c2.y * z, c2.z * z);
    float thick = c2.w * len;
    int steps = (int)c3.z;
    float jit = ign(sv.xy);
    // Slope-scaled bias: a grazing light needs more room before a nearer depth counts as an occluder, and the depth of a
    // slanted surface changes by `slope` per pixel, which is how far off the nearest-pixel lookup of a ray point can be.
    float sx = min(dl, dr);
    float sy = min(du, dd);
    float slope = max(sx > 1e29 ? 0.0 : sx, sy > 1e29 ? 0.0 : sy);
    float bias = c3.w * z + 0.1 * len * (1.0 - nl) + 0.5 * slope;
    float NP = dot(N, P);

    float shadow = 0.0;
    [loop]
    for (int i = 0; i < steps; i++)
    {
        float t = (float(i) + jit) / float(steps);
        float s = lerp(t, t * t, 0.5); // denser near the surface, where contact shadows live
        float3 Q = P + L * (len * s);
        if (Q.z < 1e-4)
            break;
        float2 uv = float2(Q.x / (Q.z * c3.x * c3.y) * 0.5 + 0.5, 0.5 - Q.y / (Q.z * c3.x) * 0.5);
        if (uv.x < 0.0 || uv.y < 0.0 || uv.x >= 1.0 || uv.y >= 1.0)
            break;
        int2 qp = int2(uv * c1.zw);
        float zs = D(qp);
        if (!valid(zs)) // sky or a hole: not an occluder
            continue;
        // The depth is stored per pixel but the ray point sits anywhere inside its pixel: on a slanted surface that alone
        // is a depth step per pixel (a flat plane would shadow itself at grazing angles). Move the sampled depth along the
        // local tangent plane of this pixel from the centre of the sampled pixel to the exact position of the ray point.
        float2 ndc_e = float2(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0);
        float2 uvc = (float2(qp) + 0.5) * c1.xy;
        float2 ndc_c = float2(uvc.x * 2.0 - 1.0, 1.0 - uvc.y * 2.0);
        float3 de = float3(ndc_e.x * c3.y * c3.x, ndc_e.y * c3.x, 1.0);
        float3 dc = float3(ndc_c.x * c3.y * c3.x, ndc_c.y * c3.x, 1.0);
        float nde = dot(N, de);
        float ndc_ = dot(N, dc);
        if (abs(nde) > 1e-4 && abs(ndc_) > 1e-4)
        {
            float corr = NP / nde - NP / ndc_;
            zs += clamp(corr, -2.0 * slope - 1e-4 * z, 2.0 * slope + 1e-4 * z);
        }
        float diff = Q.z - zs; // > 0: the scene surface is nearer than the ray point
        float hit = smoothstep(bias, bias + 0.15 * len, diff) * (1.0 - smoothstep(thick, 1.5 * thick, diff));
        shadow = max(shadow, hit * (1.0 - 0.5 * s));
    }
    return saturate(1.0 - c0.w * gate * shadow);
}

float ps_blur(float4 sv : SV_POSITION) : SV_Target
{
    int2 hp = int2(sv.xy);
    int2 hd = (int2(c1.zw) + 1) / 2;
    float z0 = D(full_of(hp));
    if (!valid(z0))
        return 1.0;
    float sum = 0.0;
    float wsum = 0.0;
    [unroll]
    for (int j = -1; j <= 2; j++)
    {
        [unroll]
        for (int i = -1; i <= 2; i++)
        {
            int2 q = clamp(hp + int2(i, j), int2(0, 0), hd - 1);
            float zq = D(full_of(q));
            if (!valid(zq))
                continue;
            float w = saturate(1.0 - abs(zq - z0) / (z0 * 0.03)) + 1e-3;
            sum += vis_in.Load(int3(q, 0)) * w;
            wsum += w;
        }
    }
    return wsum > 0.0 ? sum / wsum : 1.0;
}
