// HDR resolve: turns the scene-referred float16 colour buffer into the display-referred one.
//
// Mirrors src/tonemap.rs (the Rust reference). SDR (h <= 1): hard clip min(c, h) in the encoded domain.
// HDR (h > 1): sRGB EOTF extended above 1.0, per-channel smooth
// shoulder in linear light (identity below knee = max(shoulder_start * h, 1), C1 roll-off towards h), sRGB
// OETF. Input and output are gamma-encoded, like everything in the engine post chain. Alpha passes.
//
// cb0.x = shoulder_start (fraction of h), cb0.y = h (output headroom, linear, 1.0 in SDR).
// t0 = scratch copy of the scene colour. Drawn as one full-screen triangle (Draw(3, 0), no input layout).

cbuffer params : register(b0)
{
    float4 p; // x = shoulder_start fraction, y = headroom h, z = highlight boost (1..4)
};

Texture2D<float4> src : register(t0);

float3 srgb_decode(float3 v)
{
    v = max(v, 0.0);
    float3 lo = v / 12.92;
    float3 hi = pow((v + 0.055) / 1.055, 2.4);
    return (v <= 0.04045) ? lo : hi;
}

float3 srgb_encode(float3 v)
{
    v = max(v, 0.0);
    float3 lo = v * 12.92;
    float3 hi = 1.055 * pow(v, 1.0 / 2.4) - 0.055;
    return (v <= 0.0031308) ? lo : hi;
}

float3 shoulder(float3 x, float s, float h)
{
    x = max(x, 0.0);
    float r = h - s;
    if (r <= 1e-6)
        return min(x, h);
    float3 y = s + r * (1.0 - exp(-(x - s) / r));
    return (x <= s) ? x : y;
}

// Hue-preserving highlight expansion (linear light). The largest channel is multiplied by a gain
// rising smoothly from 1 at T to `gain` at TF (and staying there). Mirrors tonemap::expand (EXPAND_START, EXPAND_FULL).
static const float T = 0.4;
static const float TF = 0.9;

float3 expand(float3 x, float gain)
{
    float m = max(max(x.r, x.g), x.b);
    m = min(m, 1.25 * dot(float3(0.2126, 0.7152, 0.0722), x));
    if (m <= T || gain <= 1.0)
        return x;
    float t = saturate((m - T) / (TF - T));
    float s = t * t * (3.0 - 2.0 * t);
    float mx = max(max(x.r, x.g), x.b);
    return x * min(1.0 + (gain - 1.0) * s, max(gain / mx, 1.0));
}

struct VSOut
{
    float4 pos : SV_POSITION;
};

VSOut vs_main(uint id : SV_VertexID)
{
    float2 q = float2((id << 1) & 2, id & 2);
    VSOut o;
    o.pos = float4(q * float2(2, -2) + float2(-1, 1), 0, 1);
    return o;
}

float4 ps_main(float4 pos : SV_POSITION) : SV_Target
{
    float4 c = src.Load(int3((int2)pos.xy, 0));
    float h = p.y;
    float s = max(saturate(p.x) * h, 1.0); // knee never below SDR white (mirrors ToneParams::knee)
    float3 rgb;
    if (p.w > 1.5)
    {
        uint w, h2;
        src.GetDimensions(w, h2);
        if (pos.x < (float)w * 0.5)
            return float4(2.0, 0.0, 0.0, c.a); // debug fill: left half solid red
    }
    if (h <= 1.0)
        rgb = min(max(c.rgb, 0.0), h); // SDR: hard clip, bit-for-bit what the original chain showed
    else
    {
        float3 lin = srgb_decode(c.rgb);
        float3 ex = expand(lin, p.z);
        rgb = srgb_encode(shoulder(ex, s, h));
        if (p.w > 0.5 && max(max(ex.r, ex.g), ex.b) > max(max(lin.r, lin.g), lin.b) * 1.02)
            rgb = float3(rgb.r, rgb.g * 0.15, rgb.b * 0.15); // debug: pixels changed by the boost go red
    }
    return float4(rgb, c.a);
}
