// Lens effects (streaks, glare, ghosts) and colour grading of the 3D scene, run at the engine's pre-UI copy (ps#c0ad1686) like the
// AA. See docs/lens.md and docs/grading.md. The picture is the engine's encoded scene colour (extended sRGB, the same decode as
// final_scrgb.hlsl): everything below works on LINEAR light, 1.0 = paper white, values above 1.0 are legal highlights.
//
// Passes (all full-screen triangles into mod-owned targets, only Load() so no sampler state is touched):
//   ps_cover    full -> 1/16: share of the pixels above COVER_FRACTION x the threshold (every second pixel, encoded compare only)
//   ps_area_h / ps_area_v     box mean of that share over a square of WINDOW_MULT x lens_max_size (cov16 -> tmp16 -> cov16)
//   ps_down4    full -> 1/4: threshold (energy above the threshold, hue kept; alpha = energy above the ghost threshold),
//               the threshold raised for saturated cool colours; Karis weighted 2x2 groups (a single hot pixel does not flicker)
//   ps_down8    1/4 -> 1/8: Karis weighted 2x2, times the point-source weight of the bright area around it (big bright
//               areas such as a glowing creature are not sources)
//   ps_glare_h / ps_glare_v   isotropic soft glare, 9 taps per direction (src8 -> ping -> glare)
//   ps_streak0/1/2            horizontal streak: three 7-tap passes with the step growing x3 (dual-kawase style stretch),
//                             exponential falloff (src8 -> ping -> pong -> ping)
//   ps_composite              scene + lens (+ ghosts) -> slider grade -> LUT -> re-encode. A pixel nothing changes keeps its exact value.
//
// Slots: b0 constants, t10 scene colour, t11 1/4, t12 1/8 source, t13 ping, t14 glare, t15 pong, t16 3D LUT, t17 coverage /
// area (1/16), t18 area temp (1/16).
//
// b0 (9 float4):
//   g_full   = (w, h, 1/w, 1/h)                 of the picture
//   g_sz     = (w4, h4, w8, h8)                 of the 1/4 and 1/8 targets
//   g_lens   = (strength, ghost, threshold, streak step unit)   threshold: absolute linear (already x the highlight gain)
//   g_lens2  = (glare step, encoded ghost threshold, encoded threshold, lens gain)
//   g_gradeA = (saturation, contrast, vibrance, LUT strength)
//   g_gains  = (r, g, b gain of temperature / tint, LUT size)
//   g_gradeB = (shadows, highlights, flags, 0)  flags: 1 lens, 2 slider grade, 4 LUT
//   g_lens3  = (cool gain x colour reject, encoded coverage threshold, area tap step in 1/16 texels (0 = no size test),
//               area units per fill)
//   g_sz16   = (w16, h16, 0, 0)                 of the 1/16 targets

cbuffer C : register(b0)
{
    float4 g_full;
    float4 g_sz;
    float4 g_lens;
    float4 g_lens2;
    float4 g_gradeA;
    float4 g_gains;
    float4 g_gradeB;
    float4 g_lens3;
    float4 g_sz16;
};

Texture2D<float4> t_color : register(t10);
Texture2D<float4> t_d4 : register(t11);
Texture2D<float4> t_src8 : register(t12);
Texture2D<float4> t_ping : register(t13);
Texture2D<float4> t_glare : register(t14);
Texture2D<float4> t_pong : register(t15);
Texture3D<float4> t_lut : register(t16);
Texture2D<float4> t_cov : register(t17);
Texture2D<float4> t_tmp : register(t18);

static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);
static const float MAX_ADD = 0.6;
static const float GLARE_WEIGHT = 0.35;
static const float3 WARM_WHITE = float3(1.0, 0.92, 0.80);
// Mirrors of lens.rs (CHROMA_RAMP, AREA_RAMP, AREA_TAPS, COVER_GRID).
static const float CHROMA_LO = 0.25;
static const float CHROMA_HI = 0.75;
static const float AREA_LO = 1.0;
static const float AREA_HI = 2.5;
static const int AREA_TAPS = 16;
static const int COVER_GRID = 8;

struct VSOut { float4 pos : SV_Position; };

VSOut vs_main(uint id : SV_VertexID)
{
    float2 p = float2((id << 1) & 2, id & 2);
    VSOut o;
    o.pos = float4(p * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    return o;
}

float3 sane(float3 c)
{
    c = isnan(c) ? float3(0.0, 0.0, 0.0) : c;
    return clamp(c, 0.0, 60000.0);
}

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

// ---- bilinear Load (no sampler state is touched) -------------------------------------------------------------------------------

float4 bil(Texture2D<float4> t, float2 p, float2 dim)
{
    p -= 0.5;
    float2 f = frac(p);
    int2 a = int2(floor(p));
    int2 mx = int2(dim) - 1;
    int2 z = int2(0, 0);
    float4 v00 = t.Load(int3(clamp(a, z, mx), 0));
    float4 v10 = t.Load(int3(clamp(a + int2(1, 0), z, mx), 0));
    float4 v01 = t.Load(int3(clamp(a + int2(0, 1), z, mx), 0));
    float4 v11 = t.Load(int3(clamp(a + int2(1, 1), z, mx), 0));
    return lerp(lerp(v00, v10, f.x), lerp(v01, v11, f.x), f.y);
}

// ---- lens: which pixels are sources ----------------------------------------------------------------------------------------------

// 0 for white, warm and unsaturated colours, up to 1 for saturated cool ones (blue, cyan, violet, green). lens.rs cool_penalty.
float cool_penalty(float3 c)
{
    float mx = max(c.r, max(c.g, c.b));
    if (!(mx > 0.0))
        return 0.0;
    float mn = max(min(c.r, min(c.g, c.b)), 0.0);
    float chroma = smoothstep(CHROMA_LO, CHROMA_HI, (mx - mn) / mx);
    float warm = saturate(2.0 * (c.r - c.b) / mx);
    return chroma * (1.0 - warm);
}

// Share of the pixels of a 16x16 block that are bright (largest ENCODED channel above the encoded coverage threshold; the encoding
// is monotonic, so no decode). Every second pixel on a staggered grid: a bright AREA is what is measured, not a single pixel.
float4 ps_cover(float4 pos : SV_Position) : SV_Target
{
    int2 q = int2(pos.xy) * 16;
    int2 mx = int2(g_full.xy) - 1;
    float n = 0.0;
    [loop] for (int j = 0; j < COVER_GRID; j++)
    {
        [unroll] for (int i = 0; i < COVER_GRID; i++)
        {
            int2 p = q + int2(2 * i + (j & 1), 2 * j);
            if (all(p <= mx))
            {
                float3 enc = sane(t_color.Load(int3(p, 0)).rgb);
                n += (max(enc.r, max(enc.g, enc.b)) > g_lens3.y) ? 1.0 : 0.0;
            }
        }
    }
    return float4(n / float(COVER_GRID * COVER_GRID), 0.0, 0.0, 0.0);
}

// Box mean over AREA_TAPS taps spaced g_lens3.z (1/16 texels) along `dir`: the two passes give the bright share of a square of
// WINDOW_MULT x lens_max_size around the texel.
float4 area_pass(Texture2D<float4> t, float2 pos, float2 dir)
{
    float acc = 0.0;
    [unroll] for (int i = 0; i < AREA_TAPS; i++)
        acc += bil(t, pos + dir * ((float(i) - 0.5 * float(AREA_TAPS - 1)) * g_lens3.z), g_sz16.xy).r;
    return float4(acc / float(AREA_TAPS), 0.0, 0.0, 0.0);
}

float4 ps_area_h(float4 pos : SV_Position) : SV_Target { return area_pass(t_cov, pos.xy, float2(1.0, 0.0)); }
float4 ps_area_v(float4 pos : SV_Position) : SV_Target { return area_pass(t_tmp, pos.xy, float2(0.0, 1.0)); }

// Point-source weight of a 1/8 texel: 1 when the bright area around it is at most about one lens_max_size square, 0 when it is
// 2.5 of them or more (a glowing body, a bright wall, the edges and limbs of both). lens.rs area_weight.
float point_weight(float2 pos8)
{
    if (g_lens3.z <= 0.0)
        return 1.0;
    float fill = saturate(bil(t_cov, pos8 * 0.5, g_sz16.xy).r);
    return 1.0 - smoothstep(AREA_LO, AREA_HI, fill * g_lens3.w);
}

// ---- lens: source, downsample --------------------------------------------------------------------------------------------------

// Threshold of one full-resolution pixel: rgb = the energy above the streak threshold (hue kept), a = the energy above the ghost
// threshold. The threshold is raised for saturated cool colours (lens.rs source_threshold); the early out uses the plain one.
float4 lens_src(int2 p)
{
    int2 mx = int2(g_full.xy) - 1;
    float3 enc = sane(t_color.Load(int3(clamp(p, int2(0, 0), mx), 0)).rgb);
    float me = max(enc.r, max(enc.g, enc.b));
    if (me <= g_lens2.z)
        return float4(0.0, 0.0, 0.0, 0.0);
    float3 lin = srgb_decode(enc);
    float m = max(lin.r, max(lin.g, lin.b));
    float t = g_lens.z * (1.0 + g_lens3.x * cool_penalty(lin));
    float e = max(m - t, 0.0);
    float eg = max(m - t * 2.0, 0.0);
    return float4(lin * (e / max(m, 1.0e-6)), eg);
}

float kw(float4 v)
{
    return 1.0 / (1.0 + dot(v.rgb, LUMA));
}

float4 ps_down4(float4 pos : SV_Position) : SV_Target
{
    int2 q = int2(pos.xy) * 4;
    float4 acc = float4(0.0, 0.0, 0.0, 0.0);
    float ws = 0.0;
    [unroll] for (int gy = 0; gy < 2; gy++)
    {
        [unroll] for (int gx = 0; gx < 2; gx++)
        {
            int2 o = q + int2(gx * 2, gy * 2);
            float4 a = (lens_src(o) + lens_src(o + int2(1, 0)) + lens_src(o + int2(0, 1)) + lens_src(o + int2(1, 1))) * 0.25;
            float w = kw(a);
            acc += a * w;
            ws += w;
        }
    }
    return acc / ws;
}

float4 ps_down8(float4 pos : SV_Position) : SV_Target
{
    int2 q = int2(pos.xy) * 2;
    int2 mx = int2(g_sz.xy) - 1;
    int2 z = int2(0, 0);
    float4 acc = float4(0.0, 0.0, 0.0, 0.0);
    float ws = 0.0;
    [unroll] for (int i = 0; i < 4; i++)
    {
        float4 a = t_d4.Load(int3(clamp(q + int2(i & 1, i >> 1), z, mx), 0));
        float w = kw(a);
        acc += a * w;
        ws += w;
    }
    return acc / ws * point_weight(pos.xy);
}

// ---- glare, streak ---------------------------------------------------------------------------------------------------------------

float4 glare_pass(Texture2D<float4> t, float2 pos, float2 dir)
{
    float4 acc = float4(0.0, 0.0, 0.0, 0.0);
    float ws = 0.0;
    float step = g_lens2.x;
    [unroll] for (int i = -4; i <= 4; i++)
    {
        float w = exp(-0.5 * float(i * i) / 2.25);
        acc += bil(t, pos + dir * (float(i) * step), g_sz.zw) * w;
        ws += w;
    }
    return acc / ws;
}

float4 ps_glare_h(float4 pos : SV_Position) : SV_Target { return glare_pass(t_src8, pos.xy, float2(1.0, 0.0)); }
float4 ps_glare_v(float4 pos : SV_Position) : SV_Target { return glare_pass(t_ping, pos.xy, float2(0.0, 1.0)); }

float4 streak_pass(Texture2D<float4> t, float2 pos, float mul)
{
    float4 acc = float4(0.0, 0.0, 0.0, 0.0);
    float ws = 0.0;
    float step = g_lens.w * mul;
    [unroll] for (int i = -3; i <= 3; i++)
    {
        float w = exp(-0.45 * abs(float(i)));
        acc += bil(t, pos + float2(float(i) * step, 0.0), g_sz.zw) * w;
        ws += w;
    }
    return acc / ws;
}

float4 ps_streak0(float4 pos : SV_Position) : SV_Target { return streak_pass(t_src8, pos.xy, 1.0); }
float4 ps_streak1(float4 pos : SV_Position) : SV_Target { return streak_pass(t_ping, pos.xy, 3.0); }
float4 ps_streak2(float4 pos : SV_Position) : SV_Target { return streak_pass(t_pong, pos.xy, 9.0); }

// ---- composite -----------------------------------------------------------------------------------------------------------------------

// The source colour pulled towards warm white, never dominated by blue; the energy (largest channel) is kept.
float3 lens_tint(float3 c)
{
    float m = max(c.r, max(c.g, c.b));
    if (!(m > 0.0))
        return float3(0.0, 0.0, 0.0);
    float3 n = c / m;
    n = lerp(n, WARM_WHITE, 0.65);
    n.b = min(n.b, 0.85 * max(n.r, n.g));
    float k = max(n.r, max(n.g, n.b));
    return n / k * m;
}

// Two mirrored, shrunken copies of the very bright sources about the screen centre (scale -0.3 and -0.7), a small radial
// chromatic split (the colour channels sample at slightly different scales), warm tints, no blue.
float3 ghosts(float2 uv)
{
    float3 sum = float3(0.0, 0.0, 0.0);
    float2 d = uv - 0.5;
    [unroll] for (int k = 0; k < 2; k++)
    {
        float s = (k == 0) ? -0.3 : -0.7;
        float wk = (k == 0) ? 0.6 : 0.35;
        float3 tint = (k == 0) ? float3(1.0, 0.82, 0.58) : float3(0.95, 0.88, 0.70);
        float3 e = float3(0.0, 0.0, 0.0);
        [unroll] for (int ch = 0; ch < 3; ch++)
        {
            float ss = s * (1.0 + 0.07 * float(ch - 1));
            float2 q = 0.5 + d / ss;
            if (all(q >= 0.0) && all(q <= 1.0))
                e[ch] = bil(t_glare, q * g_sz.zw, g_sz.zw).a;
        }
        sum += wk * tint * e;
    }
    return sum;
}

float contrast_scale(float Y, float c)
{
    if (abs(c - 1.0) < 1.0e-6 || Y <= 1.0e-6)
        return 1.0;
    float P = 0.18;
    float K = 1.0;
    float fK = P * pow(K / P, c);
    float f = (Y <= K) ? P * pow(Y / P, c) : fK + c * fK / K * (Y - K);
    return f / Y;
}

float3 sat_mix(float3 c, float Y, float k)
{
    return max(Y + (c - Y) * k, 0.0);
}

float3 grade(float3 c)
{
    c *= g_gains.rgb;
    c *= contrast_scale(dot(LUMA, c), g_gradeA.y);
    float Y = dot(LUMA, c);
    float wS = 1.0 - smoothstep(0.02, 0.35, Y);
    float wH = smoothstep(0.3, 1.0, Y);
    c *= (1.0 + g_gradeB.x * 0.6 * wS) * (1.0 + g_gradeB.y * 0.6 * wH);
    c = sat_mix(c, dot(LUMA, c), g_gradeA.x);
    float mx = max(c.r, max(c.g, c.b));
    float mn = min(c.r, min(c.g, c.b));
    float s = (mx - mn) / max(mx, 1.0e-6);
    return sat_mix(c, dot(LUMA, c), 1.0 + g_gradeA.z * (1.0 - s) * 0.75);
}

// Trilinear lookup, x in 0..1 per axis (r, g, b).
float3 lut_tri(float3 x)
{
    float n = g_gains.w;
    float3 t = saturate(x) * (n - 1.0);
    float3 f0 = floor(t);
    float3 f = t - f0;
    int3 i0 = int3(f0);
    int3 mx = int3(n - 1.0 + 0.5, n - 1.0 + 0.5, n - 1.0 + 0.5);
    int3 i1 = min(i0 + 1, mx);
    float3 r = float3(0.0, 0.0, 0.0);
    [unroll] for (int k = 0; k < 8; k++)
    {
        int3 sel = int3(k & 1, (k >> 1) & 1, (k >> 2) & 1);
        int3 idx = int3(sel.x ? i1.x : i0.x, sel.y ? i1.y : i0.y, sel.z ? i1.z : i0.z);
        float w = (sel.x ? f.x : 1.0 - f.x) * (sel.y ? f.y : 1.0 - f.y) * (sel.z ? f.z : 1.0 - f.z);
        r += t_lut.Load(int4(idx, 0)).rgb * w;
    }
    return r;
}

// log2 shaper between 2^-8 and 2^6 on the way in, its inverse on the way out; values outside keep their ratio, so an identity
// LUT is the identity everywhere (black and super-white included).
float3 lut_apply(float3 lin)
{
    float lo = exp2(-8.0);
    float hi = exp2(6.0);
    float3 refc = clamp(lin, lo, hi);
    float3 x = (log2(refc) + 8.0) / 14.0;
    float3 y = lut_tri(x);
    return exp2(y * 14.0 - 8.0) * (lin / refc);
}

float4 ps_composite(float4 pos : SV_Position) : SV_Target
{
    int2 p = int2(pos.xy);
    float4 c = t_color.Load(int3(p, 0));
    uint flags = (uint)(g_gradeB.z + 0.5);
    float3 lin = srgb_decode(sane(c.rgb));
    bool changed = false;
    if ((flags & 1u) != 0u)
    {
        float2 uv = pos.xy * g_full.zw;
        float2 p8 = uv * g_sz.zw;
        float3 sg = bil(t_ping, p8, g_sz.zw).rgb + GLARE_WEIGHT * bil(t_glare, p8, g_sz.zw).rgb;
        float3 add = lens_tint(sg) * (g_lens.x * g_lens2.w);
        add += g_lens.y * ghosts(uv);
        float mxa = max(add.r, max(add.g, add.b));
        if (mxa > 0.0)
        {
            add *= 1.0 / (1.0 + mxa / MAX_ADD);
            lin += add;
            changed = true;
        }
    }
    if ((flags & 2u) != 0u)
    {
        lin = grade(lin);
        changed = true;
    }
    if ((flags & 4u) != 0u)
    {
        lin = lerp(lin, lut_apply(lin), g_gradeA.w);
        changed = true;
    }
    if (!changed)
        return c;
    lin = sane(lin);
    return float4(srgb_encode(lin), c.a);
}
