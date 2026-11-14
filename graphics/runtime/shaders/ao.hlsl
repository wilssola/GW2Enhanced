// Screen-space ambient occlusion over the engine's own linear depth (see docs/m2-ao.md).
//
// Three full-screen draws (one triangle each, no input layout):
//   ps_ao      depth -> raw visibility (R16_FLOAT), 16 normal-oriented taps, per-pixel rotated spiral
//   ps_blur_h  depth-aware horizontal blur of the visibility
//   ps_apply   depth-aware vertical blur, then the darkening factor, written as a MULTIPLY into the scene
//              colour (blend state: result = dest * src, RGB only, alpha untouched)
//
// Only depth RATIOS and view-space geometry are used, so the unit of the engine's linear depth does not
// matter (metres, centimetres or 0..1 all give the same result). The vertical field of view is the one
// assumption (c0.z). Invalid depth (<= 0, NaN, inf: sky, cleared areas) is never darkened.
//
// c0: x = strength (0..2), y = radius slider (0.25..4), z = tan(vertical half fov), w = width / height
// c1: x = 1/width, y = 1/height, z = width, w = height
// c2: x = max darkening at strength 1 and full occlusion, y = normal bias, z = radius base (fraction of depth
//     per slider unit), w = occlusion gain
// t0 = linear depth (R32_FLOAT), t1 = visibility (R16_FLOAT, blur and apply only)

cbuffer params : register(b0)
{
    float4 c0;
    float4 c1;
    float4 c2;
};

Texture2D<float> depth : register(t0);
Texture2D<float> vis_in : register(t1);

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

bool valid(float z)
{
    return z > 1e-5 && isfinite(z) && z < 1e30;
}

// Depth at a pixel; outside the image it is 0, which is "invalid" (never used as a neighbour or a tap).
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
    return float3(ndc.x * c0.w * c0.z * z, ndc.y * c0.z * z, z);
}

float ps_ao(float4 sv : SV_POSITION) : SV_Target
{
    int2 p = int2(sv.xy);
    float z = D(p);
    if (!valid(z))
        return 1.0;
    float3 P = VP(p, z);

    // Normal from the smaller of the two one-sided depth differences per axis.
    float zl = D(p + int2(-1, 0));
    float zr = D(p + int2(1, 0));
    float zu = D(p + int2(0, -1));
    float zd = D(p + int2(0, 1));
    float dl = valid(zl) ? abs(z - zl) : 1e30;
    float dr = valid(zr) ? abs(zr - z) : 1e30;
    float du = valid(zu) ? abs(z - zu) : 1e30;
    float dd = valid(zd) ? abs(zd - z) : 1e30;
    float3 dx = (dr < dl) ? (VP(p + int2(1, 0), zr) - P) : (P - VP(p + int2(-1, 0), zl));
    float3 dy = (dd < du) ? (VP(p + int2(0, 1), zd) - P) : (P - VP(p + int2(0, -1), zu));
    float3 N = float3(0.0, 0.0, -1.0);
    if (min(min(dl, dr), min(du, dd)) < 1e29)
    {
        float3 n = cross(dx, dy);
        float len = length(n);
        if (len > 1e-20)
            N = n / len;
        if (dot(N, P) > 0.0)
            N = -N;
    }

    // World radius is a fraction of the depth, so the projected radius is the same at every distance.
    float Rw = c0.y * c2.z * z;
    float rpx = clamp(c0.y * c2.z * c1.w / (2.0 * c0.z), 3.0, 128.0);
    float rot = 6.2831853 * frac(52.9829189 * frac(dot(sv.xy, float2(0.06711056, 0.00583715))));

    float occ = 0.0;
    [unroll]
    for (int i = 0; i < 16; i++)
    {
        float t = (float(i) + 0.5) / 16.0;
        float a = rot + float(i) * 2.39996323;
        float2 off = float2(cos(a), sin(a)) * (rpx * (0.1 + 0.9 * t));
        int2 q = p + int2(round(off));
        if (any(q < int2(0, 0)) || any(q >= int2(c1.zw)))
            continue;
        float zq = D(q);
        if (!valid(zq))
            continue;
        float3 v = VP(q, zq) - P;
        float d = length(v);
        if (d < 1e-8)
            continue;
        float ndv = dot(N, v) / d;
        float fall = saturate(1.0 - d / Rw);
        occ += saturate(ndv - c2.y) * fall;
    }
    occ = occ / 16.0;
    return saturate(1.0 - occ * c2.w);
}

static const float W[5] = { 1.0, 0.923, 0.726, 0.487, 0.278 };

float blur(int2 p, int2 dir)
{
    float z0 = D(p);
    float c = vis_in.Load(int3(clamp(p, int2(0, 0), int2(c1.zw) - 1), 0));
    if (!valid(z0))
        return 1.0;
    float sum = c;
    float wsum = 1.0;
    [unroll]
    for (int k = -4; k <= 4; k++)
    {
        if (k == 0)
            continue;
        int2 q = clamp(p + dir * k, int2(0, 0), int2(c1.zw) - 1);
        float zq = D(q);
        if (!valid(zq))
            continue;
        float w = W[abs(k)] * saturate(1.0 - abs(zq - z0) / (z0 * 0.03));
        sum += vis_in.Load(int3(q, 0)) * w;
        wsum += w;
    }
    return sum / wsum;
}

float ps_blur_h(float4 sv : SV_POSITION) : SV_Target
{
    return blur(int2(sv.xy), int2(1, 0));
}

// Vertical blur: the finished visibility (1 = open, < 1 = occluded) in its own target. The light-adjust
// substitute (light_adjust_hdr.hlsl, data path) samples it as t6.
float ps_blur_v(float4 sv : SV_POSITION) : SV_Target
{
    return blur(int2(sv.xy), int2(0, 1));
}

float4 ps_apply(float4 sv : SV_POSITION) : SV_Target
{
    int2 p = int2(sv.xy);
    if (c0.x < -5.0) // debug fill: top half solid black
        return (sv.y < c1.w * 0.5) ? float4(0.0, 0.0, 0.0, 1.0) : float4(1.0, 1.0, 1.0, 1.0);
    if (!valid(D(p)))
        return float4(1.0, 1.0, 1.0, 1.0);
    float v = blur(p, int2(0, 1));
    float m = saturate(abs(c0.x) * (1.0 - v));
    float f = 1.0 - c2.x * m;
    if (c0.x < 0.0) // debug view: any occlusion turns black
        f = 1.0 - saturate(m * 8.0);
    return float4(f, f, f, 1.0);
}
