// Screen-space reflections over the engine's linear depth (see docs/ssr.md).
//
// Three full-screen draws into HALF-resolution RGBA16F targets (one triangle each, no input layout):
//   ps_trace   per pixel: rebuild the view-space normal from the depth (like ao.hlsl), keep only nearly
//              horizontal, camera-facing surfaces (floors), march the reflection ray in screen space
//              (32 coarse steps, dithered start, thickness test, 6 binary refinement steps) and write
//              rgb = reflected scene colour (LINEAR light) already multiplied by every weight, a = the weight
//   ps_blur_h  depth-aware horizontal blur of rgba (roughness blur and noise removal)
//   ps_blur_v  the same, vertical
// The light-adjust substitute (light_adjust_hdr.hlsl, slot t8/b7) samples the result and adds it to the linear
// colour: lin += refl.rgb * strength. Only Load() is used (no sampler state needs saving).
//
// Camera model (same as ao.hlsl): view space x right, y UP, z forward; tan(vertical half fov) = c0.y,
// aspect = c0.z. Unrolled camera assumed (the view-space up component of the normal means "floor").
//
// c0: x = range slider (ray length factor), y = tan(half fov), z = aspect, w = debug reflectivity (> 0.5: the alpha of
//     the output is the REFLECTIVITY score instead of the weight, for the heat map of debug level 5)
// c1: x = 1/width, y = 1/height, z = width, w = height   (FULL resolution: depth and scene colour)
// c2: x = Fresnel floor, y = ray length in depths per range unit, z = weight of a sky reflection, w = max linear colour
// c3: x = target width, y = target height (half resolution), z = allowed material classes (bitmask 1 low, 2 mid,
//     4 high: the class of the RAW ALPHA of the scene colour at the receiving pixel), w = smart (0..1: strength of
//     the reflectivity score below; 0 = off, bit-identical to the plain trace)
// c4: x = hue filter (0..1, scales the green / warm rejection), y = flatness (0..1, scales the large-scale flatness
//     requirement), z = water boost (0..1; 0 = the trace below is the old one, bit for bit), w = water depth look (0..1)
// c5: x = matte cut (0..1, `ssr_matte_cut`; 0 = the score below exactly as before), y = stats grid width, z = stats grid
//     height (ps_stats only), w = unused
//
// MATTE GATE (c5.x > 0 and smart > 0; docs/ssr.md "Matte gate"). The engine renders forward into one 8-bit target: there is
// no gloss / roughness / specular buffer and the scene alpha does not separate terrain materials, so the score below can only
// judge the COLOUR, and colour is lighting: under moonlight, blue fog or a blue sky ambient, dry dirt and rock are cool and
// pale exactly like water, and in grey haze or shade they are colourless exactly like polished stone. Measured on real
// captures (colour replay, docs/ssr.md): 36 % of the ground of a night dirt path and 28 % of a foggy dirt slope scored above 0.
// The gate multiplies the raw score by lerp(1, gloss^MATTE_K, cut), where gloss is a stricter material prior:
//   1. the receiver colour is white balanced by the frame's mean colour (t3, ps_mean: 32x18 taps of valid depth), by `cut`:
//      a global light tint (moon, fog, sky ambient) is removed, water stays bluer than the frame around it;
//   2. the water / glass / ice ramps get stricter (MC_COOL_*), and very dark receivers (colour ratios of a few 8-bit codes) need
//      MC_DARK_Y of luminance;
//   3. the colourless "polished stone / metal" branch is scaled by (1 - cut): without a gloss signal a neutral smooth floor
//      cannot be told from dry ground in shade, so it no longer counts as evidence by default.
// Water (gloss 1) keeps its score and the water boost; anything else falls below the score gate (GATE0) and is EXACTLY 0.
//
// WATER AND GLASS (c4.z > 0 and smart > 0; docs/ssr.md "Water and glass"): on top of the reflectivity score, a receiver whose
// colour class is water / glass / ice (cool and coloured, the same prior as below, WITHOUT the hue-filter slider) and whose
// score is above 0 is a water receiver, wat = smoothstep(0.5, 0.9, prior) * score. For those pixels only (wat = 0 elsewhere
// changes nothing):
//   a. Fresnel: Schlick F0 + (1 - F0) (1 - n.v)^5, F0 0.02 (saturated: water) .. 0.04 (pale: glass / ice), x (1 + WATER_GAIN
//      boost), never below the old term, clamped to FRES_MAX = 0.85; blended in by wat * boost.
//   b. tint: the reflected colour is multiplied by lerp(1, receiver hue / luminance, WATER_TINT * boost * wat).
//   c. screen edge: a ray that leaves the screen (or a hit near the border) blends towards the sky found in the same column
//      (the first sky pixel from the top), weighted by the sky reflection weight c2.z, instead of fading to nothing: no black
//      band at the lake's edges.
//   d. depth look: the receiver is darkened by up to WATER_DARK_MAX * c4.w where the reflection is strong (a NEGATIVE rgb the
//      light-adjust shader adds; it is proportional to the receiver's own linear colour, never below 85 % of it).
//   e. calm / rippled: the blur passes read a second target (t2: x = wat * boost, y = ripple 0..1 from the roughness term of
//      the score). Calm water (ripple 0) keeps only CALM_OUTER of the outer blur taps (a sharper mirror), rippled water spreads
//      the taps up to RIPPLE_SPREAD times (a softer one). x = 0 everywhere else: the taps and weights are unchanged.
//
// REFLECTIVITY SCORE (smart > 0), computed at the receiving pixel in the trace pass, in [0,1]; the reflection weight
// is multiplied by lerp(1, score, smart). The engine's alpha material mask does NOT separate terrain materials (it
// marks characters and effects), so the score uses only what the picture itself shows. It is POSITIVE evidence: a
// surface reflects only if it looks like a reflective material; anything unclassified scores 0.
//   raw   = class_prior * smoothness * flatness * specular_cue        (each term in [0,1])
//   score = smoothstep(GATE0, GATE1, raw)                              (steep gate: weak evidence is EXACTLY 0)
//   1. class prior (linear light of the receiver; lerp(1, x, c4.x), so hue_filter 0 = every colour is a candidate):
//        WATER / GLASS / ICE: cool (B >= R, ramp COOL_RATIO0..1) and coloured (saturation COOL_SAT0..1): prior 1
//        WET / POLISHED stone, METAL: nearly colourless (saturation below NEUT_SAT0..1): prior NEUTRAL_PRIOR (0.8)
//        everything else (warm brown / ochre / sand / wood / rust, green, dark desaturated earth): prior 0
//        vegetation (G / max(R,B) VEG_RATIO0..1 with saturation) and warm earth (R / B WARM_RATIO0..1, R >= G >= B)
//        also veto a cool-looking value (a green with B > R), 100 %.
//   2. smoothness: standard deviation of the ENCODED luminance plus the two chroma axes (R-B and G-(R+B)/2) at two
//        scales, rough = noisy at EITHER; smoothness = 1 - rough (micro detail kills the reflection):
//        narrow: 5x5 taps 2 px apart (trace resolution), ramp ROUGH_N0..ROUGH_N1
//        wide:   9x9 taps 4 px apart (= stride 2 in trace pixels, +-16 px), ramp ROUGH_W0..ROUGH_W1
//   3. flatness (lerp(1, x, c4.y)): the normals rebuilt from the depth at 4 taps on 3 rings (FLAT_RADIUS px, scaled by
//        height / 1080 when taller) must agree with the centre one (the depth-normal variance); up to 95 % off.
//   4. specular cue: bright diffuse surfaces (sand, snow glare) reflect less than dark / mid metal and water:
//        1 - SPEC_MAX_LOSS * smoothstep(SPEC_Y0, SPEC_Y1, linear luminance).
// Stability: the class and the cue use a 5-tap spatial mean; the score is a pure function of the picture at trace resolution (no time input); the depth-aware blur
// passes then also smooth the mask (alpha), so a still camera gives a still score and no frame to frame flicker.
// Tuning knobs (all thresholds are in this one block):
static const float VEG_RATIO0 = 1.15, VEG_RATIO1 = 1.40;
static const float SAT0 = 0.15, SAT1 = 0.35;
static const float WARM_RATIO0 = 1.30, WARM_RATIO1 = 2.00;
static const float COOL_RATIO0 = 0.95, COOL_RATIO1 = 1.15;
static const float COOL_SAT0 = 0.08, COOL_SAT1 = 0.22;
static const float NEUT_SAT0 = 0.06, NEUT_SAT1 = 0.16;
static const float NEUTRAL_PRIOR = 0.8;
static const float ROUGH_N0 = 0.03, ROUGH_N1 = 0.10;
static const float ROUGH_W0 = 0.05, ROUGH_W1 = 0.14;
static const float3 FLAT_RADIUS = float3(4.0, 8.0, 16.0);
static const float3 FLAT_LO = float3(0.08, 0.07, 0.06);
static const float3 FLAT_HI = float3(0.30, 0.26, 0.22);
static const float SPEC_Y0 = 0.25, SPEC_Y1 = 0.80, SPEC_MAX_LOSS = 0.5;
static const float GATE0 = 0.35, GATE1 = 0.70;
static const float WATER_F0 = 0.02, GLASS_F0 = 0.04, FRES_MAX = 0.85, WATER_GAIN = 1.0;
static const float WATER_TINT = 0.35, WATER_DARK_MAX = 0.15;
static const float RIPPLE_SPREAD = 2.0, CALM_OUTER = 0.3;
static const float2 MC_COOL_RATIO = float2(1.25, 1.60); // B/R ramp of the balanced colour at cut 1 (COOL_RATIO0..1 at cut 0)
static const float2 MC_COOL_SAT = float2(0.18, 0.35);   // saturation ramp at cut 1 (COOL_SAT0..1 at cut 0)
static const float2 MC_DARK_Y = float2(0.01, 0.04);     // linear luminance a water receiver needs (scaled in by cut)
static const float2 MC_GAIN = float2(0.5, 2.0);         // limits of the white-balance gain per channel
static const float MATTE_K = 2.0;                       // gloss exponent of the gate
// Side outputs of the score / the trace (written before they are read; per invocation).
static float g_smooth = 1.0;                  // the smoothness (1 - rough) of the last reflectivity()
static float3 g_mcol = float3(0.0, 0.0, 0.0); // the 5-tap mean colour of the receiver (encoded)
static float2 g_wm = float2(0.0, 0.0);        // second output: x = wat * boost, y = ripple
static float3 g_dk = float3(0.0, 0.0, 0.0);   // water depth look: negative linear rgb added to every output
static float g_water = 0.0;                   // the water part of the matte gloss of the last reflectivity() (0 when cut is 0)
// t0 = linear depth (R32_FLOAT, full size), t1 = scene colour (trace) or the previous pass (blur)

cbuffer params : register(b0)
{
    float4 c0;
    float4 c1;
    float4 c2;
    float4 c3;
    float4 c4;
    float4 c5;
};

Texture2D<float> depth : register(t0);
Texture2D<float4> t_in : register(t1);
Texture2D<float2> t_m : register(t2); // blur passes: the water map of the trace (x = amount, y = ripple)
Texture2D<float4> t_mean : register(t3); // trace / stats: 1x1, rgb = mean linear colour of valid-depth pixels, a = their share

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

// Depth at a full-resolution pixel; outside the image it is 0 ("invalid").
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
    return float3(ndc.x * c0.z * c0.y * z, ndc.y * c0.y * z, z);
}

// View-space point -> continuous full-resolution pixel position (pixel p has its centre at p + 0.5).
float2 proj(float3 P)
{
    float2 ndc = float2(P.x / (P.z * c0.y * c0.z), P.y / (P.z * c0.y));
    return float2((ndc.x * 0.5 + 0.5) * c1.z, (0.5 - ndc.y * 0.5) * c1.w);
}

// Distance to the nearest screen border in uv units (0 at the border, 0.5 in the centre).
float border(float2 pix)
{
    float2 uv = pix * c1.xy;
    float2 e = min(uv, 1.0 - uv);
    return min(e.x, e.y);
}

float ign(float2 p)
{
    return frac(52.9829189 * frac(dot(p, float2(0.06711056, 0.00583715))));
}

// Class of the engine's raw material alpha (shader 4340ca88 thresholds): 0 low, 1 mid, 2 high.
int alpha_class(float a)
{
    return a > 0.491961 ? 2 : (a > 0.240980 ? 1 : 0);
}

// Scene colour (encoded) at a full-resolution pixel, clamped to the image.
float3 enc_col(int2 q)
{
    q = clamp(q, int2(0, 0), int2(c1.zw) - 1);
    return t_in.Load(int3(q, 0)).rgb;
}

// Class prior from the colour (linear light): water / glass / ice 1, colourless polished stone / metal NEUTRAL_PRIOR,
// anything else (warm, green, unclassified) 0. POSITIVE evidence: the default is 0.
float class_prior(float3 col)
{
    float3 lin = max(col, 0.0) * max(col, 0.0);
    float M = max(lin.r, lin.b);
    float mx = max(lin.g, M);
    float mn = min(min(lin.r, lin.g), lin.b);
    float sat = (mx - mn) / (mx + 1e-4);
    float veg = smoothstep(VEG_RATIO0, VEG_RATIO1, lin.g / (M + 1e-4)) * smoothstep(SAT0, SAT1, sat) * smoothstep(0.003, 0.02, lin.g);
    float order = smoothstep(-0.05, 0.05, (lin.r - lin.g) / (lin.r + 1e-4)) * smoothstep(-0.05, 0.05, (lin.g - lin.b) / (lin.g + 1e-4));
    float warm = smoothstep(WARM_RATIO0, WARM_RATIO1, lin.r / (lin.b + 1e-4)) * order * smoothstep(0.003, 0.02, mx);
    float veto = 1.0 - max(veg, warm);
    float cool = smoothstep(COOL_RATIO0, COOL_RATIO1, lin.b / (lin.r + 1e-4)) * smoothstep(COOL_SAT0, COOL_SAT1, sat);
    float neutral = (1.0 - smoothstep(NEUT_SAT0, NEUT_SAT1, sat)) * NEUTRAL_PRIOR;
    return max(cool, neutral) * veto;
}

// The water / glass / ice part of the class prior alone (cool and coloured, vetoed by vegetation and warm earth); `sat` is the
// saturation of the colour (water is more saturated than glass and ice).
float water_prior(float3 col, out float sat)
{
    float3 lin = max(col, 0.0) * max(col, 0.0);
    float M = max(lin.r, lin.b);
    float mx = max(lin.g, M);
    float mn = min(min(lin.r, lin.g), lin.b);
    sat = (mx - mn) / (mx + 1e-4);
    float veg = smoothstep(VEG_RATIO0, VEG_RATIO1, lin.g / (M + 1e-4)) * smoothstep(SAT0, SAT1, sat) * smoothstep(0.003, 0.02, lin.g);
    float order = smoothstep(-0.05, 0.05, (lin.r - lin.g) / (lin.r + 1e-4)) * smoothstep(-0.05, 0.05, (lin.g - lin.b) / (lin.g + 1e-4));
    float warm = smoothstep(WARM_RATIO0, WARM_RATIO1, lin.r / (lin.b + 1e-4)) * order * smoothstep(0.003, 0.02, mx);
    float cool = smoothstep(COOL_RATIO0, COOL_RATIO1, lin.b / (lin.r + 1e-4)) * smoothstep(COOL_SAT0, COOL_SAT1, sat);
    return cool * (1.0 - max(veg, warm));
}

// Matte gate (header): the gloss evidence of the receiver (encoded colour, the 5-tap mean) for matte cut `cut` > 0, in [0,1].
// 1 = water / glass / ice relative to the frame's light, (1 - cut) x NEUTRAL_PRIOR at most for colourless, 0 for the rest.
// `water` returns the water / glass / ice part alone.
float matte_gloss(float3 col, float cut, out float water)
{
    float3 lin = max(col, 0.0) * max(col, 0.0);
    float4 m = t_mean.Load(int3(0, 0, 0));
    if (m.a > 0.0 && all(isfinite(m.rgb)))
    {
        float ym = dot(float3(0.2126, 0.7152, 0.0722), max(m.rgb, 0.0));
        float3 gain = clamp(ym / max(m.rgb, 1e-6), MC_GAIN.x, MC_GAIN.y);
        lin *= lerp(float3(1.0, 1.0, 1.0), gain, cut);
    }
    float M = max(lin.r, lin.b);
    float mx = max(lin.g, M);
    float mn = min(min(lin.r, lin.g), lin.b);
    float sat = (mx - mn) / (mx + 1e-4);
    float veg = smoothstep(VEG_RATIO0, VEG_RATIO1, lin.g / (M + 1e-4)) * smoothstep(SAT0, SAT1, sat) * smoothstep(0.003, 0.02, lin.g);
    float order = smoothstep(-0.05, 0.05, (lin.r - lin.g) / (lin.r + 1e-4)) * smoothstep(-0.05, 0.05, (lin.g - lin.b) / (lin.g + 1e-4));
    float warm = smoothstep(WARM_RATIO0, WARM_RATIO1, lin.r / (lin.b + 1e-4)) * order * smoothstep(0.003, 0.02, mx);
    float veto = 1.0 - max(veg, warm);
    float Y = dot(float3(0.2126, 0.7152, 0.0722), lin);
    float cool = smoothstep(lerp(COOL_RATIO0, MC_COOL_RATIO.x, cut), lerp(COOL_RATIO1, MC_COOL_RATIO.y, cut), lin.b / (lin.r + 1e-4))
               * smoothstep(lerp(COOL_SAT0, MC_COOL_SAT.x, cut), lerp(COOL_SAT1, MC_COOL_SAT.y, cut), sat)
               * lerp(1.0, smoothstep(MC_DARK_Y.x, MC_DARK_Y.y, Y), cut);
    float neutral = (1.0 - smoothstep(NEUT_SAT0, NEUT_SAT1, sat)) * NEUTRAL_PRIOR * (1.0 - cut);
    water = saturate(cool * veto);
    float g = saturate(max(cool, neutral) * veto);
    return isfinite(g) ? g : 0.0;
}

// The sky seen in the column of `pos` (continuous full-resolution position): the encoded scene colour of the first pixel
// from the top whose depth is invalid (sky / cleared), searched over the upper 30 % of the screen. false when there is none.
bool sky_above(float2 pos, out float3 sk)
{
    sk = float3(0.0, 0.0, 0.0);
    int x = clamp(int(pos.x), 0, int(c1.z) - 1);
    [loop]
    for (int i = 0; i < 6; i++)
    {
        int y = int(float(i) * c1.w * 0.06);
        if (!valid(D(int2(x, y))))
        {
            sk = enc_col(int2(x, y));
            return true;
        }
    }
    return false;
}

// Smoothness at two scales (luminance and chroma variance): 1 = smooth, 0 = micro detail.
float3 chroma_axes(float3 c)
{
    return float3(dot(float3(0.2126, 0.7152, 0.0722), c), c.r - c.b, c.g - 0.5 * (c.r + c.b));
}

float rough_score(int2 p)
{
    float3 s1 = 0.0, s2 = 0.0;
    [unroll]
    for (int j = -2; j <= 2; j++)
    {
        [unroll]
        for (int i = -2; i <= 2; i++)
        {
            float3 l = chroma_axes(enc_col(p + int2(2 * i + (j & 1), 2 * j)));
            s1 += l;
            s2 += l * l;
        }
    }
    float3 mean = s1 / 25.0;
    float3 v = max(s2 / 25.0 - mean * mean, 0.0);
    float sd = sqrt(v.x + 0.5 * (v.y + v.z));
    float rough = smoothstep(ROUGH_N0, ROUGH_N1, sd);
    if (rough < 0.99)
    {
        float3 w1 = 0.0, w2 = 0.0;
        [loop]
        for (int y = -4; y <= 4; y++)
        {
            [unroll]
            for (int x = -4; x <= 4; x++)
            {
                float3 l = chroma_axes(enc_col(p + int2(4 * x, 4 * y)));
                w1 += l;
                w2 += l * l;
            }
        }
        float3 wm = w1 / 81.0;
        float3 wv = max(w2 / 81.0 - wm * wm, 0.0);
        float wsd = sqrt(wv.x + 0.5 * (wv.y + wv.z));
        rough = max(rough, smoothstep(ROUGH_W0, ROUGH_W1, wsd));
    }
    return 1.0 - rough;
}

// View-space normal (facing the camera) of the pixel q, same method as in ps_trace; false when it cannot be built.
bool normal_at(int2 q, out float3 N)
{
    N = float3(0.0, 1.0, 0.0);
    float z = D(q);
    if (!valid(z))
        return false;
    float3 P = VP(q, z);
    float zl = D(q + int2(-1, 0));
    float zr = D(q + int2(1, 0));
    float zu = D(q + int2(0, -1));
    float zd = D(q + int2(0, 1));
    float dl = valid(zl) ? abs(z - zl) : 1e30;
    float dr = valid(zr) ? abs(zr - z) : 1e30;
    float du = valid(zu) ? abs(z - zu) : 1e30;
    float dd = valid(zd) ? abs(zd - z) : 1e30;
    if (min(min(dl, dr), min(du, dd)) > 1e29)
        return false;
    float3 dx = (dr < dl) ? (VP(q + int2(1, 0), zr) - P) : (P - VP(q + int2(-1, 0), zl));
    float3 dy = (dd < du) ? (VP(q + int2(0, 1), zd) - P) : (P - VP(q + int2(0, -1), zu));
    float3 n = cross(dx, dy);
    float len = length(n);
    if (len < 1e-20)
        return false;
    N = n / len;
    if (dot(N, P) > 0.0)
        N = -N;
    return true;
}

// Large-scale flatness: 1 = the normals around agree with the centre one on all three rings.
float flat_score(int2 p, float3 Nc)
{
    float sc = max(1.0, c1.w / 1080.0);
    float f = 1.0;
    [unroll]
    for (int r = 0; r < 3; r++)
    {
        int rad = max(1, int(FLAT_RADIUS[r] * sc + 0.5));
        float sum = 0.0, n = 0.0;
        [unroll]
        for (int k = 0; k < 4; k++)
        {
            int2 o = (k == 0) ? int2(rad, 0) : ((k == 1) ? int2(-rad, 0) : ((k == 2) ? int2(0, rad) : int2(0, -rad)));
            float3 Nt;
            if (normal_at(p + o, Nt))
            {
                sum += length(Nt - Nc);
                n += 1.0;
            }
        }
        float spread = n > 0.0 ? sum / n : 0.0;
        f *= 1.0 - 0.95 * smoothstep(FLAT_LO[r], FLAT_HI[r], spread);
    }
    return f;
}

// The reflectivity score of the receiver (see the header). col = encoded scene colour at the receiver.
float reflectivity(int2 p, float3 col, float3 Nc, float cut)
{
    // The colour class and the specular cue use a small spatial mean (centre + 4 taps 2 px away): texture grain and
    // single-pixel noise cannot flip the class, so the mask does not shimmer.
    col = (col + enc_col(p + int2(2, 0)) + enc_col(p + int2(-2, 0)) + enc_col(p + int2(0, 2)) + enc_col(p + int2(0, -2))) * 0.2;
    float s = lerp(1.0, class_prior(col), c4.x);
    g_mcol = col;
    if (s > 0.02)
    {
        g_smooth = rough_score(p);
        s *= g_smooth;
    }
    if (s > 0.02 && c4.y > 0.0)
        s *= lerp(1.0, flat_score(p, Nc), c4.y);
    float3 lin = max(col, 0.0) * max(col, 0.0);
    float Y = dot(float3(0.2126, 0.7152, 0.0722), lin);
    s *= 1.0 - SPEC_MAX_LOSS * smoothstep(SPEC_Y0, SPEC_Y1, Y);
    g_water = 0.0;
    if (cut > 0.0)
    {
        // Matte gate (header): flat, smooth and pale is not enough; the receiver must look like water / glass / ice
        // under the frame's own light.
        float gl = matte_gloss(col, cut, g_water);
        s *= lerp(1.0, pow(gl, MATTE_K), cut);
    }
    return smoothstep(GATE0, GATE1, saturate(s));
}

float4 fin(float4 v)
{
    return float4(v.rgb + g_dk, v.a); // + 0 everywhere but on water with the depth look on
}

float4 trace_core(float4 sv)
{
    const float4 none = float4(0.0, 0.0, 0.0, 0.0);
    int2 hp = int2(sv.xy);
    int2 p = min(int2((float2(hp) + 0.5) * c1.zw / c3.xy), int2(c1.zw) - 1);
    float z = D(p);
    if (!valid(z))
        return none;
    // Material class of the receiver (raw alpha of the scene colour): disallowed classes do not reflect.
    float4 recv = t_in.Load(int3(p, 0));
    if (((uint(c3.z + 0.5) >> uint(alpha_class(recv.a))) & 1u) == 0u)
        return none;
    float3 P = VP(p, z);

    // Normal from the smaller of the two one-sided depth differences per axis (as ao.hlsl).
    float zl = D(p + int2(-1, 0));
    float zr = D(p + int2(1, 0));
    float zu = D(p + int2(0, -1));
    float zd = D(p + int2(0, 1));
    float dl = valid(zl) ? abs(z - zl) : 1e30;
    float dr = valid(zr) ? abs(zr - z) : 1e30;
    float du = valid(zu) ? abs(z - zu) : 1e30;
    float dd = valid(zd) ? abs(zd - z) : 1e30;
    if (min(min(dl, dr), min(du, dd)) > 1e29)
        return none;
    float3 dx = (dr < dl) ? (VP(p + int2(1, 0), zr) - P) : (P - VP(p + int2(-1, 0), zl));
    float3 dy = (dd < du) ? (VP(p + int2(0, 1), zd) - P) : (P - VP(p + int2(0, -1), zu));
    float3 n = cross(dx, dy);
    float len = length(n);
    if (len < 1e-20)
        return none;
    float3 N = n / len;
    if (dot(N, P) > 0.0)
        N = -N;

    // Only floors: the up component of the normal dominates (a wall has ~0, a ceiling is negative).
    float floorness = smoothstep(0.80, 0.94, N.y);
    if (floorness <= 0.0)
        return none;

    // Reflectivity of the receiver (1 when smart is 0). Pixels that stop here still carry it for the heat map.
    float refl = 1.0;
    if (c3.w > 0.0)
        refl = lerp(1.0, reflectivity(p, recv.rgb, N, c5.x), c3.w);
    const float4 none_d = float4(0.0, 0.0, 0.0, c0.w > 0.5 ? refl : 0.0);

    // Conservative Fresnel: a low floor, growing towards grazing angles.
    float3 V = normalize(P);
    float ndv = saturate(dot(N, -V));
    float fres = c2.x + (0.6 - c2.x) * pow(1.0 - ndv, 4.0); // capped at 0.6 even at grazing angles
    float3 R = reflect(V, N);

    // Water and glass (header): a stronger Fresnel, a tint, the sky at the screen edge, the depth look, the blur map.
    float wat = 0.0;
    float wk = 0.0;
    float3 wlin = float3(0.0, 0.0, 0.0);
    if (c4.z > 0.0 && c3.w > 0.0)
    {
        float wsat;
        float wp = water_prior(g_mcol, wsat);
        wat = smoothstep(0.5, 0.9, wp) * refl;
        if (wat > 0.0)
        {
            float f0 = lerp(GLASS_F0, WATER_F0, smoothstep(0.12, 0.30, wsat));
            float fs = min(f0 + (1.0 - f0) * pow(1.0 - ndv, 5.0), FRES_MAX);
            float target = min(max(fres, fs * (1.0 + WATER_GAIN * c4.z)), FRES_MAX);
            wk = wat * c4.z;
            fres = lerp(fres, target, wk);
            wlin = max(g_mcol, 0.0) * max(g_mcol, 0.0);
            g_wm = float2(wk, saturate((1.0 - g_smooth) * 2.0));
            g_dk = -wlin * (WATER_DARK_MAX * saturate(c4.w) * wk * floorness * saturate(fres / 0.3));
        }
    }

    // A ray heading back to the camera cannot hit anything the depth buffer shows.
    float toward = smoothstep(-0.05, 0.25, R.z);
    if (toward <= 0.0)
        return fin(none_d);

    float3 P0 = P + N * (0.004 * z);
    float3 P1 = P0 + R * (c2.y * c0.x * z);
    float nearZ = 0.1 * z;
    if (P1.z < nearZ)
        P1 = P0 + (P1 - P0) * ((P0.z - nearZ) / max(P0.z - P1.z, 1e-6));

    float2 s0 = proj(P0);
    float2 s1 = proj(P1);
    float k0 = 1.0 / P0.z;
    float k1 = 1.0 / P1.z;
    float2 d = s1 - s0;

    // Clip the screen segment to the image (1 px margin): t_end is the fraction that stays inside.
    float t_end = 1.0;
    if (d.x > 1e-6) t_end = min(t_end, (c1.z - 1.0 - s0.x) / d.x);
    if (d.x < -1e-6) t_end = min(t_end, (1.0 - s0.x) / d.x);
    if (d.y > 1e-6) t_end = min(t_end, (c1.w - 1.0 - s0.y) / d.y);
    if (d.y < -1e-6) t_end = min(t_end, (1.0 - s0.y) / d.y);
    float2 dv = d * t_end;
    float dk = (k1 - k0) * t_end;
    float seg = length(dv);
    if (t_end <= 0.0 || seg < 2.0)
        return fin(none_d);

    const int STEPS = 32;
    float jit = 0.1 + 0.9 * ign(sv.xy);
    float prev_t = 0.0;
    float hit_t = -1.0;
    bool sky = false;
    float zb = 0.0;
    [loop]
    for (int i = 0; i < STEPS; i++)
    {
        float t = (float(i) + jit) / float(STEPS);
        float2 pos = s0 + dv * t;
        float rz = 1.0 / (k0 + dk * t);
        float zs = D(int2(floor(pos)));
        if (!valid(zs))
        {
            // The ray entered a sky / cleared pixel: it escaped above all geometry.
            hit_t = t;
            sky = true;
            break;
        }
        float dz = abs(1.0 / (k0 + dk * (t + 1.0 / float(STEPS))) - rz);
        float diff = rz - zs;
        if (diff > 0.002 * zs && diff < 0.04 * zs + dz)
        {
            // Refine between the last step in front of the surface and this one.
            float lo = prev_t, hi = t;
            [unroll]
            for (int r = 0; r < 6; r++)
            {
                float m = 0.5 * (lo + hi);
                float zm = D(int2(floor(s0 + dv * m)));
                if (valid(zm) && 1.0 / (k0 + dk * m) > zm + 0.002 * zm)
                    hi = m;
                else
                    lo = m;
            }
            hit_t = hi;
            zb = zs;
            break;
        }
        prev_t = t;
    }
    if (hit_t < 0.0)
    {
        // Water: a ray that left the screen upwards reflects the sky of its column instead of nothing.
        if (wk > 0.0 && t_end < 1.0 && R.y > 0.0)
        {
            float3 sk;
            if (sky_above(s0 + dv, sk))
            {
                float ws = fres * floorness * toward * smoothstep(0.0, 0.04, t_end) * smoothstep(0.0, 0.05, border(s0)) * c2.z * wk;
                float3 sl = min(sk * sk, float3(c2.w, c2.w, c2.w));
                if (!any(isnan(sl)))
                    return fin(float4(sl * ws, c0.w > 0.5 ? refl : ws));
            }
        }
        return fin(none_d);
    }

    float2 hpix = s0 + dv * hit_t;
    int2 q = clamp(int2(floor(hpix)), int2(0, 0), int2(c1.zw) - 1);
    float3 col = t_in.Load(int3(q, 0)).rgb;
    float3 lin = min(col * col, float3(c2.w, c2.w, c2.w)); // gamma 2.0 decode, firefly clamp
    if (any(isnan(lin)))
        return fin(none_d);

    float t_full = hit_t * t_end;
    float fade_end = 1.0 - smoothstep(0.55, 1.0, t_full);
    float fade_start = smoothstep(0.0, 0.04, t_full);
    float edge_h = smoothstep(0.0, 0.10, border(hpix));
    float fade_edge = edge_h * smoothstep(0.0, 0.05, border(s0));
    if (wk > 0.0)
    {
        // In-medium tint (the receiver's hue, luminance normalised), and the sky instead of a fade near the border.
        float3 wt = clamp(wlin / max(dot(float3(0.2126, 0.7152, 0.0722), wlin), 1e-4), 0.5, 1.8);
        lin *= lerp(float3(1.0, 1.0, 1.0), wt, WATER_TINT * wk);
        float3 sk;
        if (edge_h < 1.0 && sky_above(hpix, sk))
        {
            float3 sl = min(sk * sk, float3(c2.w, c2.w, c2.w));
            if (!any(isnan(sl)))
            {
                lin = lerp(sl, lin, lerp(1.0, edge_h, wk));
                fade_edge = lerp(fade_edge, smoothstep(0.0, 0.05, border(s0)), wk);
            }
        }
    }
    float w = fres * floorness * toward * fade_end * fade_start * fade_edge * (sky ? c2.z : 1.0);
    w *= refl;
    return fin(float4(lin * w, c0.w > 0.5 ? refl : w));
}

struct TraceOut
{
    float4 c : SV_Target0;
    float2 m : SV_Target1;
};

TraceOut ps_trace(float4 sv : SV_POSITION)
{
    TraceOut o;
    o.c = trace_core(sv);
    o.m = g_wm;
    return o;
}

float4 blur(int2 hp, int2 dir)
{
    int2 hsz = int2(c3.xy);
    int2 pc = min(int2((float2(hp) + 0.5) * c1.zw / c3.xy), int2(c1.zw) - 1);
    float z0 = D(pc);
    if (!valid(z0))
        return float4(0.0, 0.0, 0.0, 0.0);
    float4 sum = t_in.Load(int3(clamp(hp, int2(0, 0), hsz - 1), 0));
    float wsum = 1.0;
    // Water: calm keeps only a share of the outer taps, ripples spread them (both 1 / 1 where the map is zero).
    float2 wm = t_m.Load(int3(clamp(hp, int2(0, 0), hsz - 1), 0));
    float spread = 1.0 + (RIPPLE_SPREAD - 1.0) * wm.x * wm.y;
    float outer = lerp(1.0, CALM_OUTER, wm.x * (1.0 - wm.y));
    const float W[5] = { 1.0, 0.923, 0.726, 0.487, 0.278 };
    [unroll]
    for (int k = -4; k <= 4; k++)
    {
        if (k == 0)
            continue;
        int2 q = clamp(hp + dir * int(round(float(k) * spread)), int2(0, 0), hsz - 1);
        int2 pq = min(int2((float2(q) + 0.5) * c1.zw / c3.xy), int2(c1.zw) - 1);
        float zq = D(pq);
        if (!valid(zq))
            continue;
        float w = W[abs(k)] * outer * saturate(1.0 - abs(zq - z0) / (z0 * 0.03));
        sum += t_in.Load(int3(q, 0)) * w;
        wsum += w;
    }
    return sum / wsum;
}

float4 ps_blur_h(float4 sv : SV_POSITION) : SV_Target
{
    return blur(int2(sv.xy), int2(1, 0));
}

float4 ps_blur_v(float4 sv : SV_POSITION) : SV_Target
{
    return blur(int2(sv.xy), int2(0, 1));
}

// 1x1 pass before the trace (matte gate, header): mean LINEAR colour of the pixels with valid depth on a 32x18 grid
// (sky and cleared pixels excluded, each tap clamped to c2.w), a = the share of valid taps (0 = nothing known: no balance).
float4 ps_mean(float4 sv : SV_POSITION) : SV_Target
{
    float3 s = float3(0.0, 0.0, 0.0);
    float n = 0.0;
    [loop]
    for (int j = 0; j < 18; j++)
    {
        [loop]
        for (int i = 0; i < 32; i++)
        {
            int2 q = min(int2((float2(i, j) + 0.5) / float2(32.0, 18.0) * c1.zw), int2(c1.zw) - 1);
            if (!valid(D(q)))
                continue;
            float3 c = enc_col(q);
            float3 l = min(max(c, 0.0) * max(c, 0.0), float3(c2.w, c2.w, c2.w));
            if (any(isnan(l)))
                continue;
            s += l;
            n += 1.0;
        }
    }
    return n > 0.0 ? float4(s / n, n / 576.0) : float4(0.0, 0.0, 0.0, 0.0);
}

// Telemetry (status line, every few seconds): one sample per pixel of a c5.y x c5.z grid over the picture.
// x = 1 when the sample is a floor the trace would score, y = its score with the matte cut, z = the score without it (cut 0),
// w = the water part of the matte gloss (0..1; computed with cut 1 when the cut is 0, so the class is always reported).
float4 ps_stats(float4 sv : SV_POSITION) : SV_Target
{
    int2 p = min(int2(sv.xy / c5.yz * c1.zw), int2(c1.zw) - 1);
    float4 recv = t_in.Load(int3(p, 0));
    float3 N;
    if (!normal_at(p, N) || ((uint(c3.z + 0.5) >> uint(alpha_class(recv.a))) & 1u) == 0u)
        return float4(0.0, 0.0, 0.0, 0.0);
    if (smoothstep(0.80, 0.94, N.y) <= 0.0)
        return float4(0.0, 0.0, 0.0, 0.0);
    float with_cut = reflectivity(p, recv.rgb, N, c5.x);
    float wat = g_water;
    float without = reflectivity(p, recv.rgb, N, 0.0);
    if (c5.x <= 0.0)
        matte_gloss(g_mcol, 1.0, wat);
    return float4(1.0, with_cut, without, wat);
}
