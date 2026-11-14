// Replacement for the engine's final pass (ps#afdc14cb) when the swapchain is float16 scRGB.
//
// The original is: sample t0 at TEXCOORD0.xy (s0); mul by cb0[0].x (a gain); write. Same inputs, same
// slots, same cbuffer here. The scene texture holds display-referred, gamma-encoded values in [0, h]
// (the resolve, or plain SDR content when the gate is off); the scRGB swapchain wants linear light
// where 1.0 = 80 nits. So after the gain: extended sRGB EOTF (the power segment continues above 1.0,
// the same function as resolve.hlsl / tonemap.rs), then * paper_white_nits / 80, then clamped to the
// configured peak (peak_nits / 80) per channel: whatever pass sequence, hidden UI or closed gate
// produced the input, the picture can never go above the peak. Alpha passes.
//
// The tone shoulder lives here too (the retired in-place resolve pass used to do it, see src/datapath.rs):
// above KNEE (linear, 1.0 = paper white) the largest channel rolls off smoothly towards HEADROOM
// (= peak / paper white), keeping the hue. UI and ordinary scenes stay below KNEE (>= 1.0) and are untouched.
//
// PAPER_WHITE_SCALE (= paper_white_nits / 80), PEAK_SCALE (= peak_nits / 80), HEADROOM and KNEE are
// prepended as #defines by output::final_hlsl.
//
// Optional build switches (prepended by output::final_hlsl_with; all off = exactly the scRGB shader above):
//   HDR10_OUTPUT 1  the swapchain is R10G10B10A2 / HDR10: the scRGB-unit result is converted BT.709 -> BT.2020, x 80 (nits), PQ
//                   (ST.2084) encoded, plus a 1-LSB (10 bit) triangular dither (DITHER 0 turns the dither off, for the tests).
//   CALIB_PATTERN n 1 peak blocks, 2 paper white swatches, 3 black level patches: the picture is replaced by a test pattern in
//                   absolute nits in the same output encoding (src/calib.rs has the layout; the overlay only adds labels).
//   SDR_GAMMA g     (sdr_gamma, default 2.2 while the mod is on; not defined = the piecewise sRGB curve, as Windows shows SDR):
//                   the SDR range 0..1 of the input is decoded with a pure power g instead of the piecewise sRGB EOTF. The game
//                   was made for SDR monitors, which decode about gamma 2.2; the piecewise sRGB curve has a linear toe that is
//                   several times brighter near black (encoded 0.05: 0.39 % vs 0.14 % of white), which lifts every shadow into a
//                   grey veil at a high paper white. Above 1.0 the extended sRGB curve stays (C0 at 1.0, the slopes 2.2 vs 2.28
//                   almost match), so highlights, the shoulder and the peak are unchanged; 1.0 is still exactly paper white.

#ifndef HDR10_OUTPUT
#define HDR10_OUTPUT 0
#endif
#ifndef DITHER
#define DITHER 1
#endif
#ifndef CALIB_PATTERN
#define CALIB_PATTERN 0
#endif

#ifndef PAPER_WHITE_SCALE
#define PAPER_WHITE_SCALE 2.5375
#endif
#ifndef PEAK_SCALE
#define PEAK_SCALE 12.5
#endif
#ifndef HEADROOM
#define HEADROOM 1.0
#endif
#ifndef KNEE
#define KNEE 1.0e9
#endif

cbuffer c0 : register(b0)
{
    float4 g; // x = gain (cb0[0].x of the original)
};

Texture2D<float4> t0 : register(t0);
SamplerState s0 : register(s0);

float3 srgb_decode(float3 v)
{
    v = max(v, 0.0);
    float3 lo = v / 12.92;
    float3 hi = pow((v + 0.055) / 1.055, 2.4);
    return (v <= 0.04045) ? lo : hi;
}

// The display decode of the picture (see SDR_GAMMA above). NaN and negatives give 0.
float3 decode_in(float3 v)
{
    float3 s = srgb_decode(v);
#ifdef SDR_GAMMA
    float3 p = pow(saturate(v), SDR_GAMMA);
    s = (v <= 1.0) ? p : s;
#endif
    return s;
}

#if HDR10_OUTPUT
uint hash_u(uint x)
{
    x ^= x >> 16;
    x *= 0x7feb352dU;
    x ^= x >> 15;
    x *= 0x846ca68bU;
    x ^= x >> 16;
    return x;
}

// v80: linear BT.709, 1.0 = 80 nits (the scRGB unit). Returns PQ code values of BT.2020 primaries.
float3 encode_out(float3 v80, float2 pos)
{
    const float3x3 M = float3x3(0.6274039, 0.3292830, 0.0433131,
                                0.0690973, 0.9195404, 0.0113623,
                                0.0163914, 0.0880133, 0.8955953);
    float3 nits = max(mul(M, v80), 0.0) * 80.0;
    float3 y = saturate(nits / 10000.0);
    float3 yp = pow(y, 0.1593017578125);
    float3 code = pow((0.8359375 + 18.8515625 * yp) / (1.0 + 18.6875 * yp), 78.84375);
#if DITHER
    uint2 u = (uint2)pos;
    uint h = hash_u(u.x + hash_u(u.y));
    float r1 = (h & 0xffffU) * (1.0 / 65536.0);
    float r2 = (h >> 16) * (1.0 / 65536.0);
    code = saturate(code + (r1 + r2 - 1.0) * (1.0 / 1023.0));
#endif
    return code;
}
#else
float3 encode_out(float3 v80, float2 pos)
{
    return v80;
}
#endif

#if CALIB_PATTERN != 0
// Index of the cell under uv (-1 = background). Mirrors calib::cell_at.
float calib_cell(float2 uv, float x0, float w, float n, float2 row)
{
    if (uv.y < row.x || uv.y >= row.y)
        return -1.0;
    float t = (uv.x - x0) / w;
    if (t < 0.0)
        return -1.0;
    float i = floor(t);
    if (i >= n)
        return -1.0;
    float f = t - i;
    return (f >= 0.075 && f < 0.925) ? i : -1.0;
}

// Absolute nits of the pattern at uv. Mirrors calib::nits_of.
float calib_nits(float2 uv)
{
#if CALIB_PATTERN == 1
    static const float N[8] = { 400.0, 600.0, 800.0, 1000.0, 1400.0, 2000.0, 3000.0, 4000.0 };
    float i = calib_cell(uv, 0.1, 0.1, 8.0, float2(0.35, 0.65));
    return i < 0.0 ? 0.0 : N[(int)i];
#elif CALIB_PATTERN == 2
    static const float S[3] = { 1.0, 0.18, 0.05 };
    float i = calib_cell(uv, 0.2, 0.2, 3.0, float2(0.3, 0.7));
    return i < 0.0 ? 0.0 : min(S[(int)i] * PAPER_WHITE_SCALE * 80.0, PEAK_SCALE * 80.0);
#else
    static const float B[8] = { 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0 };
    float i = calib_cell(uv, 0.1, 0.1, 8.0, float2(0.35, 0.65));
    return i < 0.0 ? 0.0 : B[(int)i];
#endif
}
#endif

float4 final_scrgb(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_Target
{
#if CALIB_PATTERN != 0
    float n = calib_nits(uv);
    return float4(encode_out(float3(n, n, n) / 80.0, pos.xy), 1.0);
#endif
    float4 c = t0.Sample(s0, uv) * g.x;
    float3 lin = decode_in(c.rgb);
    // Very bright saturated colour bleaches towards white in proportion to its energy (nothing up to 1.5x paper
    // white, so the UI and ordinary scenes are untouched; 55 % at 8x). A display cannot show a saturated colour at
    // its full white peak anyway (it clips or tone-maps each channel on its own), so without this a bright cyan
    // light comes out as flat neon; with it the hot core goes white and keeps its tint around it.
    float m0 = max(max(lin.r, lin.g), lin.b);
    float Yl = dot(float3(0.2126, 0.7152, 0.0722), lin);
    lin = lerp(lin, float3(Yl, Yl, Yl), 0.4 * saturate((m0 - 1.5) / (m0 + 1.5)));
    float m = max(max(lin.r, lin.g), lin.b);
    if (m > KNEE)
    {
        float r = max(HEADROOM - KNEE, 1.0e-4);
        float m2 = KNEE + r * (1.0 - exp(-(m - KNEE) / r));
        lin *= m2 / m;
    }
#if HDR10_OUTPUT
    return float4(encode_out(min(lin * PAPER_WHITE_SCALE, PEAK_SCALE), pos.xy), 1.0);
#else
    return float4(min(lin * PAPER_WHITE_SCALE, PEAK_SCALE), c.a);
#endif
}
