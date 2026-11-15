// Light halo: a separate, controllable glow around light sources (see docs/halo.md).
//
// Computed once per frame, right before the light-adjust draw, from the scene colour (gamma ~2.0 encoded,
// linear = encoded^2). Eight full-screen draws (one triangle each, no input layout, only Load: no sampler
// state of the game is touched):
//   ps_mean      scene -> mean of the largest channel per 16x16 block (R16_FLOAT, 1/16): "what is around here"
//   ps_source    scene + mean -> glow SOURCE energy, 8x8 pixel tent per 1/4 texel (energy conserving, no flicker)
//   ps_down      13-tap downsample 1/4 -> 1/8 -> 1/16 -> 1/32
//   ps_up        tent upsample, ADDED into the next larger level (blend ONE, ONE), weight c0.x
//   ps_up_final  the same into the 1/4 level, which also stores the local mean in alpha (debug view only)
//
// Source energy of a pixel (linear light, m = largest channel, chroma = (max - min) / max):
//   e = max(m - 0.6, 0) * saturate(2 * chroma)                 saturated bright things (fire, lamps, magic)
//     + max(m - max(1.6, 1.6 * local_mean), 0)                 bright neutral spots above 1.6x paper white; a sunlit
//       white coat or armour (about 1.0 .. 1.4) and big areas (sky) do not; colour counts only from chroma 0.25 on
//       (a white surface under warm light has about 0.1 .. 0.2)
//   clamped to 8; the colour keeps the pixel's hue: source = linear * e / m.
// light_adjust_hdr.hlsl has a copy of source_of() for its debug view: keep both identical.
//
// c0.x = weight of an upsample draw, c0.yz = size of the target being drawn (down and up passes).

cbuffer params : register(b0)
{
    float4 c0;
};

Texture2D<float4> t0 : register(t0);
Texture2D<float4> t1 : register(t1);

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

// Bilinear fetch with clamp-to-edge, built from Load. `p` is in texel units, texel centres at +0.5.
float4 bilerp(Texture2D<float4> t, float2 p)
{
    uint w, h;
    t.GetDimensions(w, h);
    int2 mx = int2(w, h) - 1;
    float2 q = p - 0.5;
    float2 f = frac(q);
    int2 i0 = int2(floor(q));
    int2 a = clamp(i0, int2(0, 0), mx);
    int2 b = clamp(i0 + int2(1, 1), int2(0, 0), mx);
    float4 v00 = t.Load(int3(a.x, a.y, 0));
    float4 v10 = t.Load(int3(b.x, a.y, 0));
    float4 v01 = t.Load(int3(a.x, b.y, 0));
    float4 v11 = t.Load(int3(b.x, b.y, 0));
    return lerp(lerp(v00, v10, f.x), lerp(v01, v11, f.x), f.y);
}

// Linear light of a scene pixel; NaN, negative and absurd values are tamed (max returns the non-NaN operand).
float3 scene_lin(int2 p)
{
    uint w, h;
    t0.GetDimensions(w, h);
    int2 q = clamp(p, int2(0, 0), int2(w, h) - 1);
    float3 c = min(max(t0.Load(int3(q.x, q.y, 0)).rgb, 0.0), 8.0);
    return c * c;
}

float3 source_of(float3 lin, float mean)
{
    float m = max(max(lin.r, lin.g), lin.b);
    float mn = min(min(lin.r, lin.g), lin.b);
    float chroma = m > 0.0001 ? (m - mn) / m : 0.0;
    float e = max(m - 0.6, 0.0) * saturate(2.5 * (chroma - 0.25)) + max(m - max(1.6, 1.6 * mean), 0.0);
    e = min(e, 8.0);
    return m > 0.0001 ? lin * (e / m) : float3(0.0, 0.0, 0.0);
}

// 1/16: mean of the (clamped) largest channel over the 16x16 block of the scene.
float4 ps_mean(VSOut i) : SV_Target
{
    int2 base = int2(i.pos.xy) * 16;
    float sum = 0.0;
    for (int y = 0; y < 16; y++)
        for (int x = 0; x < 16; x++)
        {
            float3 l = scene_lin(base + int2(x, y));
            sum += min(max(max(l.r, l.g), l.b), 4.0);
        }
    return float4(sum / 256.0, 0.0, 0.0, 1.0);
}

// 1/4: source energy, 8x8 pixel tent centred on the 4x4 block (weights 0.125 .. 0.875, sum 4 per axis).
float4 ps_source(VSOut i) : SV_Target
{
    int2 t = int2(i.pos.xy);
    int2 base = t * 4 - 2;
    float mean = bilerp(t1, (float2(t) * 4.0 + 2.0) / 16.0).x;
    float3 sum = float3(0.0, 0.0, 0.0);
    for (int y = 0; y < 8; y++)
    {
        float wy = 1.0 - abs(float(y) - 3.5) / 4.0;
        for (int x = 0; x < 8; x++)
        {
            float wx = 1.0 - abs(float(x) - 3.5) / 4.0;
            sum += source_of(scene_lin(base + int2(x, y)), mean) * (wx * wy);
        }
    }
    return float4(sum / 16.0, 0.0);
}

// 13-tap downsample (four overlapping 2x2 boxes in the middle, four corner groups), taps by bilinear Load.
float4 ps_down(VSOut i) : SV_Target
{
    uint w, h;
    t0.GetDimensions(w, h);
    float2 c = i.pos.xy * (float2(w, h) / c0.yz); // destination texel centre in source texel units
    float4 e = bilerp(t0, c);
    float4 a = bilerp(t0, c + float2(-2.0, -2.0)) + bilerp(t0, c + float2(2.0, -2.0)) + bilerp(t0, c + float2(-2.0, 2.0)) + bilerp(t0, c + float2(2.0, 2.0));
    float4 b = bilerp(t0, c + float2(0.0, -2.0)) + bilerp(t0, c + float2(-2.0, 0.0)) + bilerp(t0, c + float2(2.0, 0.0)) + bilerp(t0, c + float2(0.0, 2.0));
    float4 j = bilerp(t0, c + float2(-1.0, -1.0)) + bilerp(t0, c + float2(1.0, -1.0)) + bilerp(t0, c + float2(-1.0, 1.0)) + bilerp(t0, c + float2(1.0, 1.0));
    return e * 0.125 + a * 0.03125 + b * 0.0625 + j * 0.125;
}

// Tent upsample of t0 (the next smaller level), scaled by the weight; blended ONE, ONE by the host.
float4 up(float2 pos, float2 dst_dims)
{
    uint w, h;
    t0.GetDimensions(w, h);
    float2 c = pos * (float2(w, h) / dst_dims);
    float4 s = 4.0 * bilerp(t0, c);
    s += 2.0 * (bilerp(t0, c + float2(1.0, 0.0)) + bilerp(t0, c + float2(-1.0, 0.0)) + bilerp(t0, c + float2(0.0, 1.0)) + bilerp(t0, c + float2(0.0, -1.0)));
    s += bilerp(t0, c + float2(1.0, 1.0)) + bilerp(t0, c + float2(-1.0, 1.0)) + bilerp(t0, c + float2(1.0, -1.0)) + bilerp(t0, c + float2(-1.0, -1.0));
    return s * (c0.x / 16.0);
}

float4 ps_up(VSOut i) : SV_Target
{
    return up(i.pos.xy, c0.yz);
}

float4 ps_up_final(VSOut i) : SV_Target
{
    float4 g = up(i.pos.xy, c0.yz);
    g.a = bilerp(t1, i.pos.xy / 4.0).x; // local mean at this texel (pos is already centred: x + 0.5)
    return g;
}
