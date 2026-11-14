// HDR replacement for the engine pixel shader ps#f5cd4832 (#13 tone/exposure remap).
//
// This is the shader that produces the scene colour every later pass of the post chain reads (combine,
// anti-aliasing, copy, UI composite, final pass), so the effects that must reach the picture live HERE
// (see src/datapath.rs for why an extra full-screen pass could not be made to show):
//   * ambient occlusion: the visibility texture computed from the engine's linear depth (t6) darkens the
//     scene colour before the exposure remap, like contact shadow in the ambient light;
//   * highlight boost (HDR output only): lights and bright effects get a hue-preserving gain after the
//     remap, so they come out brighter than SDR white and feed the bloom with more energy.
//
// The remap itself is the original math with the same input signature, t0/t1/s0/s1 slots and cb0 layout;
// the only change to it is that the upper clamps are gone in HDR (the original used mad_sat on the remapped
// luminance and on the final colour; here both keep only a lower bound of 0 with max(x, 0), which also turns
// NaN into 0 because HLSL max returns the non-NaN operand). Alpha stays saturated (mov_sat o0.w).
//
// Extra inputs, bound by the mod at draw time (b6, t6). UNBOUND THEY READ ZERO, which means "no boost, no
// AO, no SDR clamp", i.e. exactly the behaviour pinned by the equivalence tests:
//   mp.x = boost gain (<= 1: none)      mp.y = AO strength (0: none)
//   mp.z = SDR clamp (> 0.5: clamp like the original and skip the boost)
//   mp.w = debug (1: boosted pixels red, occluded pixels blue, halo sources green; 2: left half red, top right black;
//          3: material classes, the raw scene alpha tints the pixel 55 %: low <= 0.241 blue, mid green, high > 0.492 red;
//          4: sun shafts: sky tinted cyan (alpha of t9), shaft light yellow, magenta cross at the sun (shafts.rs);
//          5: reflectivity heat map, the alpha of t8 (ssr.rs writes the 0..1 reflectivity score there while level 5 is
//          active) over the scene dimmed to 30 %: blue 0, cyan, green, yellow, red 1; non-floor pixels stay dim)
//   mp2.x = halo strength (0: none; the halo texture t7 is only read when this is above 0)
//   mp2.y = halo radius slider (informational: the reach is built into t7)
//   mp2.z = fur strength (0: off, nothing is read or changed; the look applies where the fur mask t20 is set, see fur_look;
//           only a diagnostic fur view of b12 runs fur_look at strength 0, and then it only paints the view)
//   mp2.w = fur debug (1: the pixels of the fur mask t20 are painted magenta, also with strength 0; see docs/draw-diagnostic.md)
// Rim light on characters (docs/rim.md), b11 + t23, bound by the mod only while a rim_ps draw was masked this frame (unbound = zero = off):
//   rp.x = rim strength (0: the whole branch is skipped, bit-identical)   rp.y = rim width in px (1..8)
//   rp.z = subsurface strength   rp.w = debug (1: the rim mask is painted cyan)   rp2.x = 2 tan(half fov) / height (grazing term)
// Live fur look (docs/fur-look.md), b12, bound by the mod at every draw of this shader. UNBOUND IT READS ZERO, and furp2.w (the "values are live"
// flag, 1 when bound by the mod) is then 0: every parameter takes its DEFAULT, so an unbound b12 is the default fur look (pinned by a test):
//   furp.x = fur_amp (0.32)   furp.y = fur_scale (1)   furp.z = fur_length (5)   furp.w = fur_detail (1)   furp2.x = fur_rim (0.25)   furp2.y = fur_sss (1)
//   furp2.z = fur_view (0 off; 1 mesh uv, 2 strand pattern, 3 rim band, 4 strand fade, on the pixels of the fur mask only)   furp2.w = live flag
// t7 = the light halo (RGBA16F, 1/4 resolution, linear light to ADD; alpha = local mean for the debug view),
// computed by src/halo.rs right before this draw. Added to the linear colour after the AO and the gamma
// decode and BEFORE the bleach and the exposure remap, so the remap, the bloom, the combine and the tone
// shoulder all see it. Unbound it reads zero.
//
// cb0[0].x = scale of the dark curve, cb0[1].x = its exponent (clamped to >= 1e-5).
// t0 = scene colour (gamma ~2.0), t1 = 1x1 weights: .y dark, .z bright, .w mid (.x is not read here).

cbuffer cb0 : register(b0)
{
    float4 dark_scale; // .x
    float4 dark_exp;   // .x
};

cbuffer mod_params : register(b6)
{
    float4 mp;
    float4 mp2;
};

Texture2D t0 : register(t0);
Texture2D t1 : register(t1);
Texture2D<float> ao_vis : register(t6);
// Fog release of the AO (fog.rs): t3 = 3x1 state (texel 0 = fog colour F rgb + confidence, texel 1 = reference depth zref,
// texel 2 = the aerial perspective's haze colour H rgb + confidence; a 2x1 state reads texel 2 as zero = no aerial),
// t4 = the engine's linear depth. Unbound they read zero = "F unknown": only the blue-water rule below applies.
Texture2D<float4> fog_state : register(t3);
Texture2D<float> fog_depth : register(t4);
Texture2D<float4> glow : register(t7);
// Fur mask (furmask.rs): R16G16B16A16_UNORM, per pixel: rg = frac of the mesh uv, b = 1 when rg is valid (0 for a draw whose
// vertex shader has no usable uv: mask only), a = 1 where a draw the fur selector picked (fur_ps / fur_vs / fur_min_vcb)
// covered the pixel this frame. Unbound it reads zero (GetDimensions gives 0): no fur, no debug paint. Any size (read with
// Load at the pixel's uv).
Texture2D<float4> fur_mask : register(t20);
// Rim mask (furmask.rs, second channel): R8_UNORM, screen size, 1 where a rim_ps draw (the character) is visible. Unbound reads zero.
Texture2D<float> rim_mask : register(t23);
cbuffer rim_params : register(b11)
{
    float4 rp;
    float4 rp2;
};
// Shell fur (shellfur.rs, docs/shell-fur.md): t24 = the shell target (R32_FLOAT holding packed uint keys: 0 = no strand, else
// hq << 24 | a << 20 | dx << 10 | dy with hq = 1 + round(h * 125) the height of the highest strand fragment at the pixel (0 root .. 1
// tip; hq 1 = the base layer), a = its coverage in 1/15 steps and dx / dy the offset to its root (the anchor) in quarter pixels + 512).
// b13: shp.x = on (0: the branch is skipped, bit-identical), shp.y = root darkening, shp.z = back light, shp.w = debug (1: the shell
// pixels are painted red at the root .. yellow at the tip; 2: a colour per fur draw, shell_debug_draws); shp2.x = detail (how strongly the strands keep the body's own features
// clear). Unbound both read zero = off.
Texture2D<float> shell_tex : register(t24);
// v11 (shellfur.rs, docs/shell-fur.md "v11"): t25 = the SKIN target (R8_UNORM, screen size): per pixel how much the visible surface is
// proven fur skin by the shell classification (a fur draw, a fur island, a fur texel, the visible surface's own uv in the fur mask),
// or a 1x1 zero texture (the shells on, nothing recorded this frame). It gates the old fur look (fur_look) and the rim light
// (rim_look): hair cards, armour, horns, claws and the cut-out pixels of alpha-tested cards get neither. Unbound (the shells off / not
// ready) = 1: the whole mask, as before v11.
Texture2D<float> fur_skin : register(t25);
cbuffer shell_params : register(b13)
{
    float4 shp;
    float4 shp2;
};

// Live fur look parameters (datapath.rs, docs/fur-look.md). Unbound = zero = furp2.w 0 = the defaults of fur_live().
cbuffer fur_look_params : register(b12)
{
    float4 furp;
    float4 furp2;
};

// Auto exposure and local tone mapping (exposure.rs, docs/exposure.md): t5 = 1x1 state (x = EV applied, already smoothed),
// t21 = base layer of the scene luminance at 1/16 resolution (x = edge-aware blurred log2 luminance, y = raw log2 luminance,
// z = share of valid (not sky) pixels), b9.x = auto exposure on (> 0.5), b9.y = local tone exponent k (0 = off), b9.z = knee
// (linear, 1 = SDR white). Unbound they read zero = both off: bit-identical to the shader without them.
cbuffer exposure_params : register(b9)
{
    float4 xp;
};
Texture2D<float4> ex_state : register(t5);
Texture2D<float4> lt_base : register(t21);

// Screen-space reflections (ssr.rs): t8 = reflected scene colour, LINEAR light, already weighted (Fresnel, floor
// test, fades); b7.x = strength (0 = off). Unbound they read zero = off.
cbuffer ssr_params : register(b7)
{
    float4 sp;
};
Texture2D<float4> refl : register(t8);
Texture2D<float4> shafts : register(t9);
cbuffer shaft_params : register(b8)
{
    float4 hp; // x = sun shaft strength (0 = off)
};
// Contact shadows and aerial perspective (contact.rs, aerial.rs, docs/contact-shadows.md, docs/aerial.md): b10 = x contact strength
// (max darkening of the diffuse light, 0 = off), y aerial strength (0 = off), z aerial half distance (m), w metres per 1.0 of the
// engine's NORMALISED linear depth (1.0 = far plane / sky; depth_unit, default 1000);
// t22 = the contact visibility (R16_FLOAT, HALF scene size, 1 = lit). Unbound they read zero = both off, bit-identical.
cbuffer atmos_params : register(b10)
{
    float4 cp;
};
Texture2D<float> contact_vis : register(t22);
static const float AERIAL_NEAR = 15.0; // metres: nothing nearer is ever touched
static const float AERIAL_CAP = 0.35;  // the largest share of the haze colour
static const float AERIAL_SKY = 0.98;  // normalised depth of the sky / far plane (the engine writes 1.0): never hazed (aerial::SKY_DEPTH)
SamplerState s0 : register(s0);
SamplerState s1 : register(s1);

struct PSIn
{
    float4 pos : SV_POSITION; // v0.xy pixel position (linear noperspective)
    float4 uv : TEXCOORD0;    // v1.xy
};

// Hue-preserving highlight expansion in linear light: the largest channel is multiplied by a gain rising
// smoothly from 1 at 0.4 to `gain` at 0.9 (and staying there). Mirrors tonemap::expand.
float3 expand(float3 x, float gain)
{
    float m = max(max(x.r, x.g), x.b);
    // The trigger is capped by the luminance (1.25x): a saturated blue / cyan / red has a large channel but little
    // light (pure blue sky: m = 1, Y = 0.07), and boosting it by its largest channel turns blue sky and clouds into
    // neon cyan. White and warm light sources (Y close to m) are boosted in full.
    m = min(m, 1.25 * dot(float3(0.2126, 0.7152, 0.0722), x));
    if (m <= 0.4 || gain <= 1.0) // 0.4 .. 0.9: tonemap::EXPAND_START / EXPAND_FULL (was 0.5 .. 1.0: almost no game pixel got there)
        return x;
    float t = saturate((m - 0.4) / 0.5);
    float s = t * t * (3.0 - 2.0 * t);
    // Bounded: the largest channel never ends above max(itself, gain) (gain <= display headroom).
    float mx = max(max(x.r, x.g), x.b);
    return x * min(1.0 + (gain - 1.0) * s, max(gain / mx, 1.0));
}

// Largest channel after the exposure `ev` (mirrors exposed_max in exposure.rs): darkening scales everything; brightening adds
// (E - 1) * h(m) with h(m) = m / (1 + (m / 0.5)^4)^(1/4): about m for dark pixels, saturating at 0.5, so highlights are not
// pushed further up (no brighter lamps) and the curve stays monotonic.
float exposed_max(float m, float ev)
{
    float e = exp2(ev);
    if (e <= 1.0)
        return m * e;
    float r = m / 0.5;
    float r2 = r * r;
    return m + (e - 1.0) * m * rsqrt(sqrt(1.0 + r2 * r2));
}

// Base layer at this pixel: joint-bilateral upsample of the 1/16 level. The 4 taps are weighted by the bilinear weight, their
// share of valid pixels and the similarity of their RAW log2 luminance to this pixel's own, so an edge between a bright and
// a dark area is never interpolated across (no halo). x = base log2 luminance, y = total weight (0 = none: sky or unbound).
float2 local_base(float2 uv, float y_scene)
{
    uint w, h;
    lt_base.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return float2(0.0, 0.0);
    int2 mx = int2(w, h) - 1;
    float2 q = uv * float2(w, h) - 0.5;
    float2 f = frac(q);
    int2 i0 = int2(floor(q));
    float lp = log2(max(y_scene, 1e-4));
    float sum = 0.0, wsum = 0.0;
    [unroll] for (int j = 0; j < 2; j++)
    {
        [unroll] for (int i = 0; i < 2; i++)
        {
            float4 t = lt_base.Load(int3(clamp(i0 + int2(i, j), int2(0, 0), mx), 0));
            float bw = (i == 0 ? 1.0 - f.x : f.x) * (j == 0 ? 1.0 - f.y : f.y);
            float d = lp - t.y;
            float wt = bw * saturate(t.z) * (exp(-d * d / (2.0 * 0.75 * 0.75)) + 0.02);
            sum += wt * t.x;
            wsum += wt;
        }
    }
    return wsum > 1e-5 ? float2(sum / wsum, wsum) : float2(0.0, 0.0);
}

// Exposure (the EV from t5) and local tone mapping, applied to the linear scene light, hue-preserving (one factor on all
// channels). Local tone: the pixel is scaled by (base / knee)^-k where its base layer (after the exposure) exceeds the knee,
// with a C1 blend over +-0.5 EV around it, so a bright area keeps its detail (the detail layer = pixel / base is untouched).
// Light sources keep their energy: where the halo's local mean says the pixel is a source (gl.a, 0 when the halo is off) the
// compression fades out. SDR output never ends above max(input, 1).
float3 exposure_tone(float3 lin, float y_scene, float2 uv, bool sdr, float halo_mean)
{
    float m = max(max(lin.r, lin.g), lin.b);
    if (!(m > 1e-6))
        return lin;
    float ev = 0.0;
    if (xp.x > 0.5)
    {
        ev = ex_state.Load(int3(0, 0, 0)).x;
        ev = isfinite(ev) ? clamp(ev, -2.0, 2.0) : 0.0;
    }
    float mo = ev != 0.0 ? exposed_max(m, ev) : m;
    if (xp.y > 0.0)
    {
        float2 b = local_base(uv, y_scene);
        if (b.y > 0.0)
        {
            float x = log2(max(exp2(b.x + ev), 1e-4) / max(xp.z, 1e-3));
            float g = x <= -0.5 ? 0.0 : (x >= 0.5 ? x : 0.5 * (x + 0.5) * (x + 0.5)); // C1 quadratic: (x + w)^2 / (4 w), w = 0.5
            float s = exp2(-xp.y * g);
            if (halo_mean > 0.0)
                s = lerp(s, 1.0, 0.75 * saturate((m - 1.35 * halo_mean - 0.25) / 0.6));
            mo *= s;
        }
    }
    if (sdr)
        mo = min(mo, max(m, 1.0));
    return lin * (mo / m);
}

// Halo texture, bilinear from Load (no dependence on the game's sampler state). Zero when unbound.
float4 sample_glow(float2 uv)
{
    uint w, h;
    glow.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return float4(0.0, 0.0, 0.0, 0.0);
    int2 mx = int2(w, h) - 1;
    float2 q = uv * float2(w, h) - 0.5;
    float2 f = frac(q);
    int2 i0 = int2(floor(q));
    int2 a = clamp(i0, int2(0, 0), mx);
    int2 b = clamp(i0 + int2(1, 1), int2(0, 0), mx);
    float4 v00 = glow.Load(int3(a.x, a.y, 0));
    float4 v10 = glow.Load(int3(b.x, a.y, 0));
    float4 v01 = glow.Load(int3(a.x, b.y, 0));
    float4 v11 = glow.Load(int3(b.x, b.y, 0));
    return lerp(lerp(v00, v10, f.x), lerp(v01, v11, f.x), f.y);
}

// Halo source energy of a pixel: the same as source_of() in halo.hlsl (keep identical). Debug view only.
float3 halo_source(float3 lin, float mean)
{
    float m = max(max(lin.r, lin.g), lin.b);
    float mn = min(min(lin.r, lin.g), lin.b);
    float chroma = m > 0.0001 ? (m - mn) / m : 0.0;
    float e = max(m - 0.6, 0.0) * saturate(2.0 * chroma) + max(m - max(0.9, 1.6 * mean), 0.0);
    e = min(e, 8.0);
    return m > 0.0001 ? lin * (e / m) : float3(0.0, 0.0, 0.0);
}

// How much of the pixel is fog or water, 0..1, without any per-map data: fog is the colour of the farthest pixels of
// the frame (F, estimated by fog.rs), so a pixel whose scene colour is close to F and that is not near the camera is
// fog, whatever the colour of the fog. Plus a mild pure-distance term for the farthest part of the picture. Only depth
// ratios are used (z / zref), so the unit of the depth does not matter. Zero when F is unknown (confidence 0).
float fog_amount_parts(float3 enc, float2 uv, out float colour_part)
{
    colour_part = 0.0;
    float4 fs = fog_state.Load(int3(0, 0, 0));
    float zref = fog_state.Load(int3(1, 0, 0)).x;
    uint dw, dh;
    fog_depth.GetDimensions(dw, dh);
    float k = saturate(fs.w);
    if (k <= 0.0 || zref <= 0.0 || dw == 0 || dh == 0)
        return 0.0;
    float z = fog_depth.Load(int3(int2(uv * float2(dw, dh)), 0));
    if (!(z > 1e-5 && z < 1e30)) // invalid depth (sky) or NaN: nothing to release, the visibility is open there
        return 0.0;
    float r = z / zref;
    float close = 1.0 - smoothstep(0.06, 0.30, distance(enc, fs.rgb));
    float cpart = close * smoothstep(0.15, 0.5, r);
    float a = max(cpart, smoothstep(0.55, 0.95, r) * 0.6);
    colour_part = saturate(cpart) * k; // only the colour match: what the aerial perspective counts as the game's own fog
    return saturate(a) * k;
}

float fog_amount(float3 enc, float2 uv)
{
    float unused;
    return fog_amount_parts(enc, uv, unused);
}

// Linear depth at this pixel; 0 when the depth is unbound or invalid (sky).
float pixel_depth(float2 uv)
{
    uint dw, dh;
    fog_depth.GetDimensions(dw, dh);
    if (dw == 0 || dh == 0)
        return 0.0;
    float z = fog_depth.Load(int3(min(int2(uv * float2(dw, dh)), int2(dw, dh) - 1), 0));
    return (z > 1e-5 && z < 1e30) ? z : 0.0;
}

// Contact visibility at this pixel (1 = lit): the half-size texture upsampled depth-aware. A half texel p was computed at
// the full pixel 2p (contact.hlsl), so each tap is weighted by the bilinear weight and by how close the depth there is to
// this pixel's depth `z`: a shadow never leaks over a depth edge. 1 when unavailable or for the sky (z = 0).
float contact_at(float2 uv, float z)
{
    uint cw, ch, dw, dh;
    contact_vis.GetDimensions(cw, ch);
    fog_depth.GetDimensions(dw, dh);
    if (cw == 0 || ch == 0 || dw == 0 || dh == 0 || z <= 0.0)
        return 1.0;
    int2 full = int2(dw, dh);
    int2 p = min(int2(uv * float2(dw, dh)), full - 1);
    float2 q = (float2(p) + 0.5) * 0.5 - 0.5;
    float2 f = frac(q);
    int2 i0 = int2(floor(q));
    float sum = 0.0, wsum = 0.0;
    [unroll] for (int j = 0; j < 2; j++)
    {
        [unroll] for (int i = 0; i < 2; i++)
        {
            int2 tp = clamp(i0 + int2(i, j), int2(0, 0), int2(cw, ch) - 1);
            float zt = fog_depth.Load(int3(min(tp * 2, full - 1), 0));
            float bw = (i == 0 ? 1.0 - f.x : f.x) * (j == 0 ? 1.0 - f.y : f.y);
            float dz = (zt - z) / (0.02 * z);
            float wt = bw * ((zt > 1e-5 && zt < 1e30 ? exp(-dz * dz) : 0.0) + 1e-3);
            sum += wt * contact_vis.Load(int3(tp, 0));
            wsum += wt;
        }
    }
    return wsum > 1e-9 ? saturate(sum / wsum) : 1.0;
}

// Aerial perspective: the linear colour `lin` of a pixel at normalised view depth `z` moves toward the haze colour H (fog_state
// texel 2: the horizon band of the sky, HELD while no sky is in view, a neutral fallback before any sky was seen; an ENCODED colour,
// confidence in w). Distance is RADIAL: metres = z * cp.w * ray, ray = |view ray| / its z with tan(half vertical fov) = 0.5 (the AO /
// contact assumption), so a far point at the screen edge counts its true distance. Nothing nearer than AERIAL_NEAR, none for the sky
// (z = 0 or the far plane) or while H is unknown; at most AERIAL_CAP; released where the pixel already has the haze colour (`enc`
// close to H: the game's own fog is never doubled) and on bluish pixels (water); a pixel above SDR white keeps its energy (the
// share is divided by 1 + (m - 1)). Before, the colour was the farthest pixels of THIS frame: with the camera tilted down (no sky)
// those are the far trees themselves, so the haze went toward their own colour and the colour match released it: no haze at all.
// Mirrors aerial::blend_at in aerial.rs.
float3 aerial(float3 lin, float3 enc, float2 uv, float z, float bluish)
{
    float4 hs = fog_state.Load(int3(2, 0, 0));
    float k = saturate(hs.w);
    if (k <= 0.0 || z <= 0.0 || z >= AERIAL_SKY || cp.w <= 0.0)
        return lin;
    uint dw, dh;
    fog_depth.GetDimensions(dw, dh);
    float aspect = dh > 0 ? float(dw) / float(dh) : 1.0;
    float2 ndc = uv * 2.0 - 1.0;
    float rx = ndc.x * 0.5 * aspect, ry = ndc.y * 0.5;
    float ray = sqrt(1.0 + rx * rx + ry * ry);
    float d = max(z * ray * cp.w - AERIAL_NEAR, 0.0);
    float h = 1.0 - exp2(-d / max(cp.z, 1.0));
    float b = min(cp.y * h, AERIAL_CAP);
    float3 hz = min(max(hs.rgb, 0.0), 2.0);
    float fog_a = 1.0 - smoothstep(0.06, 0.30, distance(enc, hz));
    b *= k * (1.0 - saturate(fog_a)) * (1.0 - saturate(bluish));
    float m = max(max(lin.r, lin.g), lin.b);
    b /= 1.0 + max(m - 1.0, 0.0);
    return lerp(lin, hz * hz, b);
}

// Integer hash of a cell, 0..1.
float hash01(int2 p)
{
    uint h = (uint)p.x * 374761393u + (uint)p.y * 668265263u;
    h = (h ^ (h >> 13)) * 1274126177u;
    h ^= h >> 16;
    return (float)(h & 0xFFFFFFu) / 16777215.0;
}

// Fur mask at a texel offset from the pixel of `uv`, 0..1 (0 where the mask is unbound; offsets clamp to the edge).
float4 fur_texel(float2 uv, int2 off)
{
    uint w, h;
    fur_mask.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return float4(0.0, 0.0, 0.0, 0.0);
    int2 p = clamp(int2(floor(uv * float2(w, h))) + off, int2(0, 0), int2(w, h) - 1); // the picture edge is not a silhouette
    return fur_mask.Load(int3(p, 0));
}

float fur_mask_at(float2 uv, int2 off)
{
    return fur_texel(uv, off).a;
}

// v11: the skin gate at the pixel of `uv` (t25): 0..1 = proven fur skin; 1 where t25 is unbound (the v10 behaviour). Any size (a 1x1
// texture is one value for the whole screen). NaN -> 0.
float fur_skin_at(float2 uv)
{
    uint w, h;
    fur_skin.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return 1.0;
    int2 p = clamp(int2(floor(uv * float2(w, h))), int2(0, 0), int2(w, h) - 1);
    return saturate(fur_skin.Load(int3(p, 0)));
}

// FUR look on the pixels of the fur mask (t20; the draws chosen by fur_ps / fur_vs / fur_min_vcb, see furmask.rs): nothing
// else is touched. `enc` = encoded scene colour (gamma ~2.0), `mask` = the fur mask at the pixel (0..1), `lum` = max channel
// of the raw colour.
//   1. detail: unsharp mask of the luminance (radius 1 px, only neighbours inside the mask), applied as a hue-preserving
//      ratio clamped to 0.85..1.15: the strands already painted in the HD texture get a little more contrast.
//   2. STRANDS (when the mask carries the mesh uv, fur_strands): procedural strands anchored to the mesh uv, so they stay on
//      the surface when the camera or the character moves and are identical frame to frame for a still picture. The lattice
//      level is picked from the uv derivative (finite differences of the 4 neighbouring mask texels), so a strand cell is
//      always ~1.5-3 px across and several times longer, whatever the uv density of the mesh; two levels are crossfaded so
//      the pattern does not pop when the camera zooms. Luminance +-fur_amp (0.32; valleys = roots darker and warm, crests = tips
//      brighter and cooler), hue kept. Faded out where the uv changes too fast (strands would be sub-pixel: aliasing /
//      moire) and where it does not change at all (flat uv = no pattern).
//   3. rim: where the mask (or the depth) ends within ~4 px, a warm desaturated glow and a feather of the outline towards
//      the neighbouring background colour; the strands make the fringe irregular.
//   4. a small warm subsurface term in the mid / dark fur.
// Released on very dark and almost-clipped pixels (lights, glows). Coverage is never changed. Without uv (fallback) only
// 1, 3 and 4 run. A flat area without uv stays flat.
// The tuning values are LIVE (b12, docs/fur-look.md): fur_amp, fur_scale, fur_length, fur_detail, fur_rim, fur_sss, read by fur_live().
// Their defaults below are the values the look had compiled in before (fur_rim: see FUR_RIM_DEFAULT).
static const float FUR_AMP_DEFAULT = 0.32;    // strand luminance modulation at strength 1 (+-)                [fur_amp]
static const float FUR_SCALE_DEFAULT = 1.0;   // multiplies the strand cell sizes FUR_CELL_A / FUR_CELL_B       [fur_scale]
static const float FUR_LONG_DEFAULT = 5.0;    // cells are this many times longer along v than wide along u    [fur_length]
static const float FUR_DETAIL_DEFAULT = 1.0;  // gain of the unsharp detail (0.7 x this)                       [fur_detail]
static const float FUR_RIM_DEFAULT = 0.25;    // share of the rim glow and feather: 1 = the outline the fur look had, 0 = none [fur_rim]
static const float FUR_SSS_DEFAULT = 1.0;     // gain of the warm subsurface term (0.12 x this)                [fur_sss]
static const float FUR_TINT = 0.05;           // strand tint variation at the default amplitude (follows fur_amp, at most twice)
static const float FUR_CELL_A = 3.0;          // px across a strand cell, coarse layer (x fur_scale)
static const float FUR_CELL_B = 1.5;          // px across a strand cell, fine layer (x fur_scale)
static const float FUR_VIEW_UV_B = 0.35;      // blue of the uv view (fur_view 1): a pixel with a valid uv is never pure red

struct FurLook
{
    float amp;
    float scale;
    float len;
    float detail;
    float rim;
    float sss;
    float view; // 0 off .. 4
};

// The live parameters, clamped to their ranges (a NaN becomes the lower end: HLSL max returns the non-NaN operand). Unbound b12 (furp2.w = 0)
// = the defaults, no view: the shader then looks exactly like the fur look that had these numbers compiled in (fur_rim aside).
FurLook fur_live()
{
    FurLook p;
    if (furp2.w > 0.5)
    {
        p.amp = clamp(furp.x, 0.0, 1.0);
        p.scale = clamp(furp.y, 0.25, 4.0);
        p.len = clamp(furp.z, 1.0, 12.0);
        p.detail = clamp(furp.w, 0.0, 2.0);
        p.rim = clamp(furp2.x, 0.0, 2.0);
        p.sss = clamp(furp2.y, 0.0, 2.0);
        p.view = clamp(round(furp2.z), 0.0, 4.0);
    }
    else
    {
        p.amp = FUR_AMP_DEFAULT;
        p.scale = FUR_SCALE_DEFAULT;
        p.len = FUR_LONG_DEFAULT;
        p.detail = FUR_DETAIL_DEFAULT;
        p.rim = FUR_RIM_DEFAULT;
        p.sss = FUR_SSS_DEFAULT;
        p.view = 0.0;
    }
    return p;
}

// The diagnostic view alone (0 when b12 is unbound): decides whether fur_look runs although the fur strength is 0.
float fur_view_id()
{
    return furp2.w > 0.5 ? clamp(round(furp2.z), 0.0, 4.0) : 0.0;
}

uint fur_hash3(uint x, uint y, uint z)
{
    uint h = x * 374761393u + y * 668265263u + z * 2246822519u;
    h = (h ^ (h >> 13)) * 1274126177u;
    h ^= h >> 16;
    return h;
}

// Periodic value noise 0..1 on a lattice of cu x cv cells per uv unit (powers of two: the pattern wraps with the uv).
float fur_vnoise(float2 uvf, uint cu, uint cv, uint salt)
{
    float2 q = float2(uvf.x * (float)cu, uvf.y * (float)cv);
    float2 fl = floor(q);
    float2 f = q - fl;
    f = f * f * (3.0 - 2.0 * f);
    uint ix = (uint)fl.x, iy = (uint)fl.y;
    uint x0 = ix & (cu - 1u), x1 = (ix + 1u) & (cu - 1u), y0 = iy & (cv - 1u), y1 = (iy + 1u) & (cv - 1u);
    float a = (float)(fur_hash3(x0, y0, salt) & 0xFFFFFFu), b = (float)(fur_hash3(x1, y0, salt) & 0xFFFFFFu);
    float c = (float)(fur_hash3(x0, y1, salt) & 0xFFFFFFu), d = (float)(fur_hash3(x1, y1, salt) & 0xFFFFFFu);
    return lerp(lerp(a, b, f.x), lerp(c, d, f.x), f.y) / 16777215.0;
}

// One strand layer: the cell size across (u) is `w` px, `len` (fur_length) times that along v. Two lattice levels (powers of two)
// are crossfaded. Returns a value centred on 0, about -1..1.
float fur_layer(float2 uvf, float gu, float gv, float w, float len, uint salt)
{
    float ku = clamp(log2(1.0 / (w * gu)), 1.0, 10.0);
    float kv = clamp(log2(1.0 / (w * len * gv)), 0.0, 9.0);
    float bu = floor(ku), bv = floor(kv);
    float tu = ku - bu, tv = kv - bv;
    // the two levels of u and of v move together: level n of u pairs with level n of v, the next with the next
    float t = 0.5 * (tu + tv);
    uint c0u = 1u << (uint)bu, c1u = 1u << (uint)min(bu + 1.0, 10.0);
    uint c0v = 1u << (uint)bv, c1v = 1u << (uint)min(bv + 1.0, 9.0);
    float n0 = fur_vnoise(uvf, c0u, c0v, salt) - 0.5;
    float n1 = fur_vnoise(uvf, c1u, c1v, salt + 7u) - 0.5;
    float norm = rsqrt(t * t + (1.0 - t) * (1.0 - t)); // the blend of two independent noises has less variance: put it back
    return 4.0 * norm * lerp(n0, n1, t);
}

// Strand value -1..1 at the mesh uv `uvf` (0..1, wraps) with the uv change per screen pixel dpx (per x) / dpy (per y); `fade`
// 0..1 is how much of the pattern survives (0 where the strands would be sub-pixel or the uv is flat). `scale` (fur_scale) multiplies
// the cell sizes, `len` (fur_length) is the cell length relative to its width.
float fur_strands(float2 uvf, float2 dpx, float2 dpy, float scale, float len, out float fade)
{
    float gu = length(float2(dpx.x, dpy.x)), gv = length(float2(dpx.y, dpy.y));
    float gmax = max(gu, gv);
    // fade: too fast a uv (strands sub-pixel at the coarsest level) or no change at all (flat uv, nothing to anchor to)
    fade = (1.0 - smoothstep(0.03, 0.08, gmax)) * smoothstep(2e-5, 1e-4, gmax);
    if (fade <= 0.0)
        return 0.0;
    float gu2 = max(gu, 0.25 * gv), gv2 = max(gv, 0.25 * gu);
    float s = 0.65 * fur_layer(uvf, gu2, gv2, FUR_CELL_A * scale, len, 11u) + 0.35 * fur_layer(uvf, gu2, gv2, FUR_CELL_B * scale, len, 101u);
    return clamp(s, -1.0, 1.0);
}

// `view_rgb` / `view_on`: the diagnostic views (fur_view 1..4, b12) PAINT the pixels of the mask (mask > 0.5) with a colour that light_adjust writes
// as the final output (like the magenta selection debug), independent of the fur strength, of the release on dark / clipped pixels and of every
// later step of the pipeline: 1 = the mesh uv as colour (u red, v green, blue FUR_VIEW_UV_B; solid red where the pixel has no valid uv),
// 2 = the strand pattern alone as grey (0.5 + 0.5 sv sfade), 3 = the rim band alone (white on black), 4 = the strand fade (white = full pattern).
float3 fur_look(float3 enc, float mask, float lum, float2 uv, float fur, out float3 view_rgb, out float view_on)
{
    view_rgb = float3(0.0, 0.0, 0.0);
    view_on = 0.0;
    if (mask <= 0.0)
        return enc;
    FurLook P = fur_live();
    bool show = P.view > 0.5 && mask > 0.5;
    float rel = (1.0 - smoothstep(0.96, 1.0, lum)) * smoothstep(0.03, 0.06, lum);
    // v11: only on proven fur skin (t25: not on hair cards, armour, horns, claws, cut-out card pixels); the diagnostic views still show
    float gate = mask * rel * fur * fur_skin_at(uv);
    if (gate <= 0.0 && !show)
        return enc;

    uint tw, th;
    t0.GetDimensions(tw, th);
    float2 st = 1.0 / float2(max(tw, 1u), max(th, 1u)); // 1 px: detail radius (a 1 px stripe still has a different neighbour)
    float2 st2 = 2.0 * st;                               // 2 px: feather taps
    const float3 LW = float3(0.299, 0.587, 0.114);
    float3 c0 = max(t0.SampleLevel(s0, uv, 0).rgb, 0.0);
    float3 ca = max(t0.SampleLevel(s0, uv + float2(st.x, 0.0), 0).rgb, 0.0);
    float3 cb = max(t0.SampleLevel(s0, uv - float2(st.x, 0.0), 0).rgb, 0.0);
    float3 cc = max(t0.SampleLevel(s0, uv + float2(0.0, st.y), 0).rgb, 0.0);
    float3 cd = max(t0.SampleLevel(s0, uv - float2(0.0, st.y), 0).rgb, 0.0);
    float4 f0 = fur_texel(uv, int2(0, 0));
    float4 fa = fur_texel(uv, int2(1, 0));
    float4 fb = fur_texel(uv, int2(-1, 0));
    float4 fc = fur_texel(uv, int2(0, 1));
    float4 fd = fur_texel(uv, int2(0, -1));
    float ma = fa.a, mb = fb.a, mc = fc.a, md = fd.a;

    // 1. detail
    float y0 = dot(c0, LW);
    float wsum = 1.0 + ma + mb + mc + md;
    float yblur = (y0 + ma * dot(ca, LW) + mb * dot(cb, LW) + mc * dot(cc, LW) + md * dot(cd, LW)) / wsum;
    float ratio = clamp(1.0 + (0.7 * P.detail) * (y0 - yblur) / max(y0, 0.05), 0.85, 1.15);
    float3 o = enc * lerp(1.0, ratio, gate);

    // 2. strands from the mesh uv: derivative by finite differences with the neighbours that belong to the mask and carry a uv
    float sv = 0.0, sfade = 0.0;
    if (f0.b > 0.5)
    {
        bool va = fa.a > 0.5 && fa.b > 0.5, vb = fb.a > 0.5 && fb.b > 0.5, vc = fc.a > 0.5 && fc.b > 0.5, vd = fd.a > 0.5 && fd.b > 0.5;
        float2 dpx = float2(0.0, 0.0), dpy = float2(0.0, 0.0);
        float2 d;
        if (va && vb)
        {
            d = fa.rg - fb.rg;
            dpx = 0.5 * (d - round(d));
        }
        else if (va)
        {
            d = fa.rg - f0.rg;
            dpx = d - round(d);
        }
        else if (vb)
        {
            d = f0.rg - fb.rg;
            dpx = d - round(d);
        }
        if (vc && vd)
        {
            d = fc.rg - fd.rg;
            dpy = 0.5 * (d - round(d));
        }
        else if (vc)
        {
            d = fc.rg - f0.rg;
            dpy = d - round(d);
        }
        else if (vd)
        {
            d = f0.rg - fd.rg;
            dpy = d - round(d);
        }
        sv = fur_strands(frac(f0.rg), dpx, dpy, P.scale, P.len, sfade);
    }
    float sg = gate * sfade;
    float k = 1.0 + P.amp * sg * sv;
    float ym = max(max(o.r, o.g), o.b);
    k = min(k, max(1.0, 1.0 / max(ym, 0.001))); // never push a bright pixel into clipping
    k = max(k, 0.7);
    float ta = FUR_TINT * min(P.amp / FUR_AMP_DEFAULT, 2.0); // the tint variation follows the amplitude (at most twice the default)
    float3 tint = lerp(float3(1.0 + ta, 1.0, 1.0 - ta), float3(1.0 - 0.6 * ta, 1.0 + 0.2 * ta, 1.0 + ta), 0.5 + 0.5 * sv);
    tint = lerp(float3(1.0, 1.0, 1.0), tint / dot(tint, LW), saturate(sg)); // luminance-neutral tint
    o = o * k * tint;

    // 3. rim: distance to the silhouette from the mask over 4 px, plus depth steps (the neighbour much farther away)
    float msum = 0.0;
    [unroll] for (int r = 1; r <= 4; r++)
        msum += fur_mask_at(uv, int2(r, 0)) + fur_mask_at(uv, int2(-r, 0)) + fur_mask_at(uv, int2(0, r)) + fur_mask_at(uv, int2(0, -r));
    float rim = saturate(1.0 - msum * (1.0 / 16.0));
    uint dw, dh;
    fog_depth.GetDimensions(dw, dh);
    if (dw > 0 && dh > 0)
    {
        int2 dp = int2(uv * float2(dw, dh));
        int2 dm = int2(dw, dh) - 1;
        float z = fog_depth.Load(int3(dp, 0));
        if (z > 1e-5 && z < 1e30)
        {
            float zf = max(max(fog_depth.Load(int3(clamp(dp + int2(4, 0), 0, dm), 0)), fog_depth.Load(int3(clamp(dp - int2(4, 0), 0, dm), 0))),
                           max(fog_depth.Load(int3(clamp(dp + int2(0, 4), 0, dm), 0)), fog_depth.Load(int3(clamp(dp - int2(0, 4), 0, dm), 0))));
            if (zf < 1e30)
                rim = max(rim, 0.5 * saturate((zf / z - 1.0) * 4.0));
        }
    }
    rim = saturate(rim * 1.6); // full strength at the middle of the band: a visible fuzzy outline
    rim *= lerp(1.0, 0.55 + 0.9 * (0.5 + 0.5 * sv), sfade); // irregular fringe
    float band = rim; // the band alone, before the strength (fur_view 3)
    rim *= gate;
    rim *= P.rim; // fur_rim: 1 = the outline this look always drew, 0 = no rim glow and no feather
    // feather: towards the colour of the neighbours OUTSIDE the mask
    float wa = 1.0 - fur_mask_at(uv, int2(2, 0)), wb = 1.0 - fur_mask_at(uv, int2(-2, 0));
    float wc = 1.0 - fur_mask_at(uv, int2(0, 2)), wd = 1.0 - fur_mask_at(uv, int2(0, -2));
    float wo = wa + wb + wc + wd;
    if (rim > 0.0 && wo > 0.0)
    {
        float3 ga = max(t0.SampleLevel(s0, uv + float2(st2.x, 0.0), 0).rgb, 0.0);
        float3 gb = max(t0.SampleLevel(s0, uv - float2(st2.x, 0.0), 0).rgb, 0.0);
        float3 gc = max(t0.SampleLevel(s0, uv + float2(0.0, st2.y), 0).rgb, 0.0);
        float3 gd = max(t0.SampleLevel(s0, uv - float2(0.0, st2.y), 0).rgb, 0.0);
        float3 bg = (wa * ga + wb * gb + wc * gc + wd * gd) / wo;
        o = lerp(o, bg, saturate(0.4 * rim * saturate(wo * 0.5)));
    }
    float yo = dot(o, LW);
    o += 0.14 * rim * lerp(float3(yo, yo, yo), float3(1.0, 0.88, 0.7), 0.6) * (0.4 + yo);

    // 4. subsurface warmth, strongest in the mid-dark fur, nothing on black or white
    float ys = saturate(dot(o, LW));
    o += (0.12 * P.sss) * gate * (1.0 - ys) * ys * float3(1.0, 0.5, 0.15);

    if (show)
    {
        view_on = 1.0;
        if (P.view < 1.5) // 1: the mesh uv as colour; a pixel without a valid uv (the mask only) is solid red
            view_rgb = f0.b > 0.5 ? float3(f0.r, f0.g, FUR_VIEW_UV_B) : float3(1.0, 0.0, 0.0);
        else if (P.view < 2.5) // 2: the strand pattern alone
        {
            float g = saturate(0.5 + 0.5 * sv * sfade);
            view_rgb = float3(g, g, g);
        }
        else if (P.view < 3.5) // 3: the rim band alone
        {
            float g = saturate(band);
            view_rgb = float3(g, g, g);
        }
        else // 4: the strand fade alone
        {
            float g = saturate(sfade);
            view_rgb = float3(g, g, g);
        }
    }
    return max(o, 0.0);
}

// ---- Rim light and subsurface glow on the pixels of the rim mask (t23, docs/rim.md) -----------------------------------------
// All in linear light. A pixel of the mask within rp.y px of the silhouette (the mask ends, or the depth jumps by more than 6 %, or the
// neighbour is sky) gets a soft rim: a relative luminance gain of at most RIM_MAX_GAIN x rim strength, weighted by how near the edge
// is and by the grazing term from the depth derivatives, scaled by how bright the background 6..10 px OUTSIDE the mask is (nothing
// in a dark scene) and tinted a little (35 %) towards that background colour. The subsurface term warms the dim-lit part of the
// mask by at most SSS_MAX_GAIN. The lift is released on dark pixels (no brighter shadows), in fog and in water (the caller passes
// the AO's release), the total relative gain is capped at RIM_MAX_GAIN and no channel is pushed above max(its pixel, 1.0).
static const float RIM_MAX_GAIN = 0.25;
static const float SSS_MAX_GAIN = 0.08;
static const float RIM_Y0 = 0.004, RIM_Y1 = 0.03;      // linear luminance ramp: no lift in the dark
static const float RIM_BG_Y0 = 0.02, RIM_BG_Y1 = 0.25; // background luminance ramp
static const float RIM_BG_NEAR = 6.0, RIM_BG_FAR = 10.0; // px outside the pixel where the background is sampled
static const float RIM_DEPTH_STEP = 1.06;              // a neighbour this much farther counts as outside (silhouette in depth)
static const int2 RIM_DIRS[8] = { int2(1, 0), int2(-1, 0), int2(0, 1), int2(0, -1), int2(1, 1), int2(1, -1), int2(-1, 1), int2(-1, -1) };

// Rim mask at a texel offset from the pixel of `uv` (0 where unbound; offsets clamp to the picture, which is not a silhouette).
float rim_mask_at(float2 uv, int2 off)
{
    uint w, h;
    rim_mask.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return 0.0;
    int2 p = clamp(int2(floor(uv * float2(w, h))) + off, int2(0, 0), int2(w, h) - 1);
    return rim_mask.Load(int3(p, 0));
}

// Linear depth at a pixel offset; 0 when invalid (sky) or unbound.
float rim_depth_at(float2 uv, int2 off)
{
    uint dw, dh;
    fog_depth.GetDimensions(dw, dh);
    if (dw == 0 || dh == 0)
        return 0.0;
    int2 p = clamp(int2(uv * float2(dw, dh)) + off, int2(0, 0), int2(dw, dh) - 1);
    float z = fog_depth.Load(int3(p, 0));
    return (z > 1e-5 && z < 1e30) ? z : 0.0;
}

// Distance in px to the nearest silhouette pixel (outside the mask, sky, or much farther); W + 1 when none within W.
float rim_edge_distance(float2 uv, float z, int W)
{
    float best = float(W) + 1.0;
    [loop]
    for (int r = 1; r <= 8; r++)
    {
        if (r > W)
            break;
        [unroll]
        for (int k = 0; k < 8; k++)
        {
            int2 o = RIM_DIRS[k] * r;
            bool outside = rim_mask_at(uv, o) < 0.5;
            if (!outside && z > 0.0)
                outside = rim_depth_at(uv, o) > z * RIM_DEPTH_STEP || rim_depth_at(uv, o) <= 0.0;
            if (outside)
                best = min(best, float(r) * (k < 4 ? 1.0 : 1.4142));
        }
        if (best <= float(r) + 1.0)
            break;
    }
    return best;
}

// 0 = facing the camera .. 1 = grazing, from the depth derivatives (smaller one-sided difference per axis). 0.5 when unknown.
float rim_grazing(float2 uv, float z)
{
    float k = rp2.x;
    if (z <= 0.0 || k <= 0.0)
        return 0.5;
    float zl = rim_depth_at(uv, int2(-1, 0)), zr = rim_depth_at(uv, int2(1, 0));
    float zu = rim_depth_at(uv, int2(0, -1)), zd = rim_depth_at(uv, int2(0, 1));
    float dx = 0.0, dy = 0.0;
    if (zl > 0.0 && zr > 0.0)
        dx = abs(z - zl) < abs(zr - z) ? z - zl : zr - z;
    else if (zl > 0.0)
        dx = z - zl;
    else if (zr > 0.0)
        dx = zr - z;
    if (zu > 0.0 && zd > 0.0)
        dy = abs(z - zu) < abs(zd - z) ? z - zu : zd - z;
    else if (zu > 0.0)
        dy = z - zu;
    else if (zd > 0.0)
        dy = zd - z;
    float sl = length(float2(dx, dy)) / (z * k);
    return 1.0 - rsqrt(1.0 + sl * sl);
}

// Brightness-weighted mean LINEAR colour of the background: 8 directions x 2 radii (6 and 10 px), only samples OUTSIDE the mask.
// `fallback` (the pixel's own colour) when every sample is inside the mask.
float3 rim_background(float2 uv, float3 fallback)
{
    uint tw, th;
    t0.GetDimensions(tw, th);
    int2 base = int2(uv * float2(tw, th));
    float3 sum = float3(0.0, 0.0, 0.0);
    float wsum = 0.0;
    [unroll]
    for (int k = 0; k < 8; k++)
    {
        [unroll]
        for (int j = 0; j < 2; j++)
        {
            float rad = j == 0 ? RIM_BG_NEAR : RIM_BG_FAR;
            int2 o = int2(round(float2(RIM_DIRS[k]) * (rad / (k < 4 ? 1.0 : 1.4142))));
            if (rim_mask_at(uv, o) < 0.5)
            {
                float3 c = min(max(t0.Load(int3(clamp(base + o, int2(0, 0), int2(tw, th) - 1), 0)).rgb, 0.0), 8.0);
                float3 l = c * c;
                float w = 0.15 + dot(float3(0.2126, 0.7152, 0.0722), l);
                sum += l * w;
                wsum += w;
            }
        }
    }
    return wsum > 0.0 ? sum / wsum : fallback;
}

float3 rim_look(float3 lin, float2 uv, float release)
{
    if (rim_mask_at(uv, int2(0, 0)) < 0.5)
        return lin;
    const float3 LW = float3(0.2126, 0.7152, 0.0722);
    float Y = dot(LW, lin);
    float gate = saturate(release) * smoothstep(RIM_Y0, RIM_Y1, Y);
    // v5 (shell-fur.md): the rim follows the fur discriminator where a fur draw with uv lies (b > 0.5): no rim on the texels it removed
    // (horn, claws, nose); elsewhere (armour, no fur mask bound, erased pixels: b = 0) it is untouched.
    float4 fdm = fur_texel(uv, int2(0, 0));
    gate *= fdm.b > 0.5 ? saturate(fdm.a) : 1.0;
    // v11: and only on proven fur skin (t25, the shell classification: not on hair cards, armour, horns, claws); unbound = 1
    gate *= fur_skin_at(uv);
    if (!(gate > 0.0)) // dark, released (fog, water) or NaN
        return lin;
    int W = clamp(int(rp.y + 0.5), 1, 8);
    float z = pixel_depth(uv);
    float best = rim_edge_distance(uv, z, W);
    float e = 1.0 - smoothstep(0.0, float(W), best - 0.5);
    float fres = e * (0.4 + 0.6 * saturate(rim_grazing(uv, z) * 2.0));
    float3 bg = rim_background(uv, lin);
    float bgY = dot(LW, bg);
    float bgk = smoothstep(RIM_BG_Y0, RIM_BG_Y1, bgY);
    float3 hue = lerp(lin / Y, bg / max(bgY, 1e-4), 0.35);
    hue /= max(dot(LW, hue), 1e-4);
    float g_rim = min(rp.x, 1.0) * RIM_MAX_GAIN * fres * bgk * gate;
    float g_sss = saturate(rp.z) * SSS_MAX_GAIN * (0.3 + 0.7 * e) * (1.0 - smoothstep(0.08, 0.45, Y)) * gate;
    float tot = g_rim + g_sss;
    if (tot > RIM_MAX_GAIN)
    {
        g_rim *= RIM_MAX_GAIN / tot;
        g_sss *= RIM_MAX_GAIN / tot;
    }
    float3 warm = float3(1.0, 0.45, 0.2);
    warm /= dot(LW, warm);
    float3 add = (hue * g_rim + warm * g_sss) * Y;
    float m = max(max(lin.r, lin.g), lin.b);
    float lim = max(m, 1.0); // never above the pixel's own largest channel or SDR white
    float mn = max(max(lin.r + add.r, lin.g + add.g), lin.b + add.b);
    if (mn > lim)
        add *= saturate((lim - m) / max(mn - m, 1e-6));
    return max(lin + add, 0.0);
}

// ---- Shell fur composite (t24 / b13, docs/shell-fur.md) ---------------------------------------------------------------------------
// The strands the shell pass left at this pixel (the highest shell wins) are painted with the colour of the body at their ROOT (the
// anchor: the engine already lit it; read bilinearly at the quarter-pixel anchor, so a root that moves by a fraction of a pixel changes
// the colour smoothly), times this pixel's own ambient / contact occlusion when the strand lies over the body (the strand sits in that
// crease too), times the strand shading 1 + rd (D0 - (1 - h)^2): darker than the body deep in the coat (the roots and the base layer,
// the skin between the strands), a little brighter at the tips, D0 = the mean depth of the visible fragments, so the AVERAGE of the fur
// stays the body's (the WARP scene: within a few %): the fur adds texture and depth, it does not grey or bleach the coat.
// Detail (shp2.x): over the body a strand only paints where the surface under it looks like its root (luminance and chroma within a
// tolerance): the eyes, the nose, the mouth, dark markings and the horns stay clear of the strands of the white fur around them instead
// of being smeared over. A strand outside the body (over the background) gets a soft back light when the background is bright (fuzzy lit
// edge; nothing in the dark), released in fog and water like the rim. A strand whose root is not on the (validated) fur mask fades out
// (bilinear mask at the anchor: hidden root, cloth; no on / off at the mask's edge). Coverage: the key's a (soft strand edges, the LOD
// fade) x 0.95 at the root .. 0.7 at the tip, everything times the fur strength. No channel is pushed above max(its source peak, 1.0).
// All in linear light; `enc` and the result are encoded (gamma 2.0).
static const float SHELL_DEPTH0 = 0.82;    // the mean "depth into the coat" (1 - h)^2 of the visible strand fragments (calibrated)
static const float SHELL_RIM = 0.35;       // back light at shell_rim 1 (share of the luminance it adds at the tip)

// The key at the pixel (0 = none: no shells, unbound, or not a key the shell pass writes).
uint shell_key_at(float2 uv)
{
    uint w, h;
    shell_tex.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return 0u;
    int2 p = clamp(int2(floor(uv * float2(w, h))), int2(0, 0), int2(w, h) - 1);
    uint key = asuint(shell_tex.Load(int3(p, 0)));
    uint k = key & 0x7FFFFFFFu; // v13: bit 31 = KEY_FRONT (the strand floats in front of the surface under it)
    if (k >= 0x7F000000u || (k >> 24) == 0u || ((k >> 20) & 15u) == 0u)
        return 0u;
    return key;
}

// Bilinear fur-mask PRESENCE and LINEAR scene colour at a sub-pixel position `px` (pixels of t0 / t20, which are the screen size).
// v7: presence = a fur draw (any texel: its uv flag b, or a for a draw without uv) is the visible surface there. The texture tests of
// the mask (a) no longer gate the shells: the strand pixel shader tests every strand at its own root uv (shellfur.hlsl strand_fur),
// with the coat-relative rule; the mask's a still drives the old fur look, the rim and fur_debug.
float shell_presence(float4 m)
{
    return max(saturate(m.a), m.b > 0.5 ? 1.0 : 0.0);
}

float shell_mask_bilinear(float2 px)
{
    uint w, h;
    fur_mask.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return 0.0;
    float2 q = px - 0.5;
    int2 b = int2(floor(q));
    float2 f = q - float2(b);
    int2 mx = int2(w, h) - 1;
    float a00 = shell_presence(fur_mask.Load(int3(clamp(b, int2(0, 0), mx), 0)));
    float a10 = shell_presence(fur_mask.Load(int3(clamp(b + int2(1, 0), int2(0, 0), mx), 0)));
    float a01 = shell_presence(fur_mask.Load(int3(clamp(b + int2(0, 1), int2(0, 0), mx), 0)));
    float a11 = shell_presence(fur_mask.Load(int3(clamp(b + int2(1, 1), int2(0, 0), mx), 0)));
    return lerp(lerp(a00, a10, f.x), lerp(a01, a11, f.x), f.y);
}

float3 shell_scene_lin(int2 p, int2 mx)
{
    float3 e = min(max(t0.Load(int3(clamp(p, int2(0, 0), mx), 0)).rgb, 0.0), 8.0);
    return e * e;
}

// v17: the bilinear root colour from the FUR pixels among the four only (weights x the fur mask's presence): at a silhouette, and
// at a distance where a strand's anchor rounds onto the pixel next to the coat, the plain bilinear read mixed the background (a dark
// floor under the tail) into the strand: the dark marks along a far tail's edge. No fur pixel among the four: the
// plain bilinear colour (as before).
float3 shell_root_colour(float2 px)
{
    uint w, h;
    t0.GetDimensions(w, h);
    float2 q = px - 0.5;
    int2 b = int2(floor(q));
    float2 f = q - float2(b);
    int2 mx = int2(w, h) - 1;
    float3 c00 = shell_scene_lin(b, mx), c10 = shell_scene_lin(b + int2(1, 0), mx);
    float3 c01 = shell_scene_lin(b + int2(0, 1), mx), c11 = shell_scene_lin(b + int2(1, 1), mx);
    float3 plain = lerp(lerp(c00, c10, f.x), lerp(c01, c11, f.x), f.y);
    uint mw, mh;
    fur_mask.GetDimensions(mw, mh);
    if (mw != w || mh != h)
        return plain;
    float m00 = shell_presence(fur_mask.Load(int3(clamp(b, int2(0, 0), mx), 0)));
    float m10 = shell_presence(fur_mask.Load(int3(clamp(b + int2(1, 0), int2(0, 0), mx), 0)));
    float m01 = shell_presence(fur_mask.Load(int3(clamp(b + int2(0, 1), int2(0, 0), mx), 0)));
    float m11 = shell_presence(fur_mask.Load(int3(clamp(b + int2(1, 1), int2(0, 0), mx), 0)));
    float w00 = (1.0 - f.x) * (1.0 - f.y) * m00, w10 = f.x * (1.0 - f.y) * m10, w01 = (1.0 - f.x) * f.y * m01, w11 = f.x * f.y * m11;
    float ws = w00 + w10 + w01 + w11;
    if (!(ws > 1e-4))
        return plain;
    return (c00 * w00 + c10 * w10 + c01 * w01 + c11 * w11) / ws;
}

// shell_debug=2 (furprobe.rs): one colour per fur draw. The fur mask's b carries the draw's tag (0.5 + k / 64, k = its index in the frame
// + 1; 1.0 = untagged). The body pixels of a draw get a light tint of its colour, the texels the fur discriminator removed (a < 0.5) turn
// black, and every shell strand takes the colour of the draw its ROOT belongs to (brighter towards the tip; white = untagged root).
// Same palette as furprobe::PALETTE (selects, not a static array: an array would add an immediate constant buffer to the substitute's
// interface, which must stay the original's plus the mod's own slots).
float3 shell_pal(uint i)
{
    i = i % 8u;
    return i == 0u ? float3(1.0, 0.0, 0.0) : i == 1u ? float3(0.0, 1.0, 0.0) : i == 2u ? float3(0.15, 0.35, 1.0) : i == 3u ? float3(1.0, 1.0, 0.0)
         : i == 4u ? float3(0.0, 1.0, 1.0) : i == 5u ? float3(1.0, 0.0, 1.0) : i == 6u ? float3(1.0, 0.5, 0.0) : float3(0.55, 0.0, 1.0);
}

bool shell_tag(float b, out float3 c)
{
    float k = round((b - 0.5) * 64.0);
    c = float3(1.0, 1.0, 1.0);
    if (!(b > 0.5) || k < 1.0 || k > 31.0)
        return false;
    c = shell_pal((uint)k - 1u);
    return true;
}

float3 shell_debug_draws(float3 o, float2 uv, uint key, float sh)
{
    uint w, h;
    fur_mask.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return o;
    int2 mx = int2(w, h) - 1;
    float4 m = fur_mask.Load(int3(clamp(int2(floor(uv * float2(w, h))), int2(0, 0), mx), 0));
    float3 c;
    if (shell_tag(m.b, c))
        o = m.a < 0.5 ? o * 0.08 : lerp(o, c, 0.3);
    if (sh >= 0.0 && key != 0u)
    {
        uint sw, sht;
        shell_tex.GetDimensions(sw, sht);
        int2 sp = clamp(int2(floor(uv * float2(sw, sht))), int2(0, 0), int2(sw, sht) - 1);
        float2 d = (float2((float)((key >> 10) & 1023u), (float)(key & 1023u)) - 512.0) * 0.25;
        float2 apx = (float2(sp) + 0.5 + d) * float2(w, h) / float2(sw, sht);
        float4 r = fur_mask.Load(int3(clamp(int2(floor(apx)), int2(0, 0), mx), 0));
        shell_tag(r.b, c);
        o = lerp(o, c * (0.55 + 0.45 * sh), 0.85);
    }
    return o;
}


// enc = the pixel after the AO / contact (encoded), raw = the engine's colour at the pixel (encoded, before any effect), shade = the
// AO x contact factor applied to this pixel (encoded scale), key = shell_key_at(uv).
float3 shell_look(float3 enc, float3 raw, float shade, uint key, float2 uv, float strength, float release, out float dbg_h)
{
    dbg_h = -1.0;
    if (key == 0u)
        return enc;
    // v13: KEY_FRONT = the surface under the strand lies clearly behind its root (the neck fur over the horn behind it): no detail test
    // against that surface's colour and not that surface's occlusion on the strand (it does not sit in that crease); the rest as before.
    bool front = (key >> 31) != 0u;
    key &= 0x7FFFFFFFu;
    uint w, h;
    shell_tex.GetDimensions(w, h);
    int2 p = clamp(int2(floor(uv * float2(w, h))), int2(0, 0), int2(w, h) - 1);
    float hh = saturate(((float)(key >> 24) - 1.0) / 125.0);
    float a = (float)((key >> 20) & 15u) / 15.0;
    float2 d = (float2((float)((key >> 10) & 1023u), (float)(key & 1023u)) - 512.0) * 0.25;
    float2 apx = float2(p) + 0.5 + d; // the root, in pixels of the shell target (= the screen)
    uint tw, th;
    t0.GetDimensions(tw, th);
    float2 sc = float2(tw, th) / float2(w, h);
    float wm = smoothstep(0.2, 0.7, shell_mask_bilinear(apx * sc)); // the root is visible fur this frame
    if (!(wm > 0.0))
        return enc;
    dbg_h = hh;
    const float3 LW = float3(0.2126, 0.7152, 0.0722);
    float3 lc = enc * enc;
    bool base = hh <= 0.0;
    bool inside = shell_presence(fur_texel(uv, int2(0, 0))) >= 0.5; // over a fur draw (also a horn / claw texel of it)
    float3 root = shell_root_colour(apx * sc);
    float3 src = base ? lc : root * ((inside && !front) ? saturate(shade) * saturate(shade) : 1.0);
    float rd = saturate(shp.y);
    float u = 1.0 - hh;
    // Self-shadowing of the coat: a fragment deeper in the coat than the average visible one is darker, a shallower one (the tips)
    // brighter; the base layer (the skin between the strands) is the deepest. On average the coat keeps the body's brightness.
    float sh = 1.0 + rd * (SHELL_DEPTH0 - u * u);
    float3 ls = src * sh;
    // Detail: over the body, only where the surface under the strand looks like its root (eyes, nose, markings stay clear).
    float keep = 1.0;
    if (!base && inside && !front && shp2.x > 0.0)
    {
        float3 rl = raw * raw;
        float yp = dot(LW, rl), yr = dot(LW, root);
        float dl = abs(log2((yp + 0.01) / (yr + 0.01)));
        float3 cp = rl / max(rl.r + rl.g + rl.b, 1e-4), cr = root / max(root.r + root.g + root.b, 1e-4);
        float dc = length(cp - cr) * 6.0;
        keep = exp2(-max(dl + dc - 0.35, 0.0) * 6.0 * saturate(shp2.x));
    }
    // Back light: only over the background (outside the body), from a bright background, strongest at the tips.
    float bgY = dot(LW, lc);
    float rim = !inside ? clamp(shp.z, 0.0, 2.0) * SHELL_RIM * hh * smoothstep(0.02, 0.25, bgY) * saturate(release) : 0.0;
    ls += rim * lerp(src, lc, 0.5);
    float m = max(max(src.r, src.g), src.b);
    float lim = max(max(m, max(max(lc.r, lc.g), lc.b)), 1.0);
    float mn = max(max(ls.r, ls.g), ls.b);
    if (mn > lim)
        ls *= lim / mn;
    float alpha = a * wm * keep * (base ? 1.0 : lerp(0.95, 0.7, hh)) * saturate(strength);
    // What shows through a partly covering strand over the body is more fur (lower strands and the undercoat), not the bare surface:
    // the base layer's darkening (else the soft strand edges would brighten the coat towards its unshaded colour).
    float3 under = (!base && inside) ? lerp(lc, lc * (1.0 + rd * (SHELL_DEPTH0 - 1.0)), wm * keep * saturate(strength)) : lc;
    return sqrt(max(lerp(under, ls, alpha), 0.0));
}

float4 light_adjust(PSIn i) : SV_Target
{
    if (mp.w > 1.5 && mp.w < 2.5) // debug fill: proves the output of this shader is what reaches the screen
    {
        if (i.uv.x < 0.5)
            return float4(1.0, 0.0, 0.0, 1.0);
        if (i.uv.y < 0.5)
            return float4(0.0, 0.0, 0.0, 1.0);
    }

    bool sdr = mp.z > 0.5;

    // 1/255 dither from a hash of the pixel position.
    float d = frac(dot(i.pos.xy + float2(1.0, 0.0), float2(0.067111, 0.005837)));
    d = frac(d * 52.982918) - 0.5;

    float e = max(dark_exp.x, 0.00001);
    float4 c = t0.Sample(s0, i.uv.xy);
    // The scene targets are float now: an effect that SUBTRACTS (or any blend that overshoots) leaves negative or NaN
    // channels that the 8-bit target clamped to 0 at every step. Squaring a negative value below would turn it into
    // a huge POSITIVE energy (flat bright rectangles around effects), so clamp first (max also turns NaN into 0).
    c.rgb = max(c.rgb, 0.0);
    float3 scene_enc = min(max(c.rgb, 0.0), 8.0); // for the halo debug view
    // Luminance of the scene colour as the exposure passes measured it (exposure.hlsl: capped, linear): joins the base layer.
    float3 scene_cap = min(c.rgb, mp.z > 0.5 ? 1.0 : 16.0);
    float y_scene = dot(float3(0.2126, 0.7152, 0.0722), scene_cap * scene_cap);

    // Ambient occlusion: the factor never goes below 0.5 and is released on bright pixels (lights and
    // effects are not ambient light, they must not be darkened by the crease they sit in).
    float vis = ao_vis.SampleLevel(s0, i.uv.xy, 0);
    float occ = saturate(mp.y * (1.0 - vis));
    float fogC;
    float fogA = fog_amount_parts(scene_enc, i.uv.xy, fogC);
    occ *= 1.0 - fogA; // fog and water of any colour: the occlusion is released (fog.rs)
    float f = 1.0 - 0.5 * occ;
    float lum = max(max(c.r, c.g), c.b);
    f = lerp(f, 1.0, saturate((lum - 0.7) * 3.0));
    // Water and fog are not ambient-lit surfaces: the engine has already blended them into the pixel, and the depth the
    // AO sees is the ground below the water / behind the haze, so darkening them paints dirty streaks on the surface.
    // Their pixels are bluish (blue clearly above the mean of red and green): the AO is released there, fully by a
    // 0.20 margin. Neutral and warm surfaces (rock, sand, grass, armour) keep the AO. This is the FALLBACK of the colour-
    // independent fog release above (fog_amount), which needs the fog colour estimate and is off when that is unknown.
    float haze = saturate((c.b - 0.5 * (c.r + c.g) - 0.04) / 0.16);
    f = lerp(f, 1.0, haze);
    c.rgb *= f;
    float shade = f; // the AO x contact factor of this pixel (encoded scale): the shell strands over the body share it
    // Contact shadows (contact.rs): a soft darkening of the diffuse light, at most cp.x (linear, converted to the encoded space
    // by the square root). The same release as the AO: the fog and water of any colour, bright pixels (lights are not ambient
    // light) and bluish water. The sky has no depth and keeps visibility 1. Aerial perspective below needs the depth too.
    float zpix = 0.0;
    if (cp.x > 0.0 || cp.y > 0.0)
        zpix = pixel_depth(i.uv.xy);
    if (cp.x > 0.0)
    {
        float cocc = saturate(cp.x * (1.0 - contact_at(i.uv.xy, zpix))) * (1.0 - fogA);
        float cf = sqrt(1.0 - cocc);
        cf = lerp(cf, 1.0, saturate((lum - 0.7) * 3.0));
        cf = lerp(cf, 1.0, haze);
        c.rgb *= cf;
        shade *= cf;
    }
    float3 fview_rgb = float3(0.0, 0.0, 0.0);
    float fview_on = 0.0;
    // Shell fur (shellfur.rs): where the shell pass left a strand fragment the shells REPLACE the old fur look (no second strand
    // pattern under the real strands; the diagnostic fur views still show).
    uint shell_key = shp.x > 0.5 ? shell_key_at(i.uv.xy) : 0u;
    if ((mp2.z > 0.0 && shell_key == 0u) || fur_view_id() > 0.5) // fur on the pixels of the fur mask (opt-in by slider, or a diagnostic fur view); an unbound mask changes nothing
        c.rgb = fur_look(c.rgb, fur_mask_at(i.uv.xy, int2(0, 0)), lum, i.uv.xy, saturate(mp2.z), fview_rgb, fview_on);
    float shell_h = -1.0;
    if (shell_key != 0u) // shell fur (shellfur.rs): strands out of the silhouette, painted with the colour at their roots
        c.rgb = shell_look(c.rgb, scene_enc, shade, shell_key, i.uv.xy, saturate(mp2.z), (1.0 - fogA) * (1.0 - haze), shell_h);

    // The 8-bit engine clipped every channel at 1.0 when it wrote the scene, so over-range colour turned white.
    // SDR output keeps exactly that. HDR output keeps the energy, but a colour that is far above SDR white
    // bleaches towards white in proportion to how far (0 at 1.0, 85 % at infinity): without it, additive
    // effects stacked on white (1, 1.5, 3) would come out as saturated paint (blue patches on white armour).
    // Within the game's own range (every channel <= 1) nothing changes.
    if (sdr)
        c.rgb = saturate(c.rgb);
    float3 lin = c.rgb * c.rgb; // gamma 2.0 decode
    if (cp.y > 0.0) // aerial perspective (aerial.rs): far pixels move a little toward the horizon / haze colour (fog.rs texel 2)
        lin = aerial(lin, scene_enc, i.uv.xy, zpix, haze);
    if (rp.x > 0.0) // rim light + subsurface glow on the pixels of the rim mask (rim.md); released in fog and water like the AO
        lin = rim_look(lin, i.uv.xy, (1.0 - fogA) * (1.0 - haze));
    // Reflections are added AFTER the AO factor (the AO must not darken them), before the exposure remap.
    float4 rfs = refl.SampleLevel(s0, i.uv.xy, 0);
    float3 rf = rfs.rgb * sp.x;
    lin += rf;
    lin = max(lin, 0.0); // the water depth look is a NEGATIVE reflection (ssr.hlsl); a NaN texel also becomes 0
    float4 gl = float4(0.0, 0.0, 0.0, 0.0);
    if (mp2.x > 0.0)
    {
        gl = sample_glow(i.uv.xy);
        lin += max(gl.rgb, 0.0) * mp2.x; // light halo (max also turns a NaN texel into 0)
    }
    // Sun shafts (shafts.rs): light from the bright sky radiating through gaps, added in linear light. Weaker on
    // pixels that are already bright so lit surfaces do not wash out. Unbound t9/b8 read zero: no shafts.
    if (hp.x > 0.0)
    {
        float3 sh = max(shafts.SampleLevel(s0, i.uv.xy, 0).rgb, 0.0);
        float lm = max(max(lin.r, lin.g), lin.b);
        lin += sh * hp.x * (1.0 - smoothstep(0.35, 1.0, lm));
    }
    if (!sdr)
    {
        float mx = max(max(lin.r, lin.g), lin.b);
        float Yl = dot(float3(0.2126, 0.7152, 0.0722), lin);
        lin = lerp(lin, float3(Yl, Yl, Yl), saturate(1.0 - 1.0 / max(mx, 1.0)) * 0.4);
    }
    if (xp.x > 0.5 || xp.y > 0.0) // auto exposure / local tone mapping (exposure.rs); unbound = untouched
        lin = exposure_tone(lin, y_scene, i.uv.xy, sdr, gl.a);
    float Y = dot(float3(0.2126, 0.7152, 0.0722), lin);

    float dark = exp2(log2(Y) * e) * dark_scale.x;
    float y4 = Y * 4.0;
    float toe = min(Y, y4 * y4 * 0.166667);
    float3 w = t1.Sample(s1, float2(0.0, 0.0)).yzw; // dark, bright, mid
    float Yraw = Y * w.z + (dark * w.x + toe * w.y);
    float Yn = sdr ? saturate(Yraw) : max(Yraw, 0.0); // was mad_sat

    float X = dot(float3(0.4124, 0.3576, 0.1805), lin);
    float Z = dot(float3(0.0193, 0.1192, 0.9505), lin);
    float sum = Z + (Y + X) + 0.00001;
    float cx = X / sum;
    float cy = Y / sum;
    float k = Yn / (cy + 0.00001);
    float3 xyz = float3(k * cx, Yn, k * ((1.0 - cx) - cy));

    float3 rgb;
    rgb.x = dot(float3(3.2405, -1.5371, -0.4985), xyz);
    rgb.y = dot(float3(-0.9693, 1.8760, 0.0416), xyz);
    rgb.z = dot(float3(0.0556, -0.2040, 1.0572), xyz);

    // Highlight boost (HDR output only). A saturated colour boosted many times over looks like neon paint, and a
    // display cannot show it anyway (its primaries limit how bright pure blue can get), so the boosted colour
    // bleaches towards white as its energy grows: nothing below 2x SDR white, 52 % at 6.4x, 85 % at most. Hot
    // cores go white and keep their tint around them, like real light.
    // The boost is for LIGHT SOURCES, not for big bright diffuse surfaces (a white armour or fur lit by the sun, which
    // the bloom then pushes above 1.0 and the boost would blow out completely). The halo pass knows what is around
    // (gl.a = local mean of the brightest channel, linear): a pixel is a source when it clearly exceeds that mean
    // (small bright spots, or anything far above its surroundings). Big uniformly bright areas get 60 % of the boost
    // (datapath::SURFACE_SHARE; was 30 %: sunlit white fur / armour then never got above paper white, the "washed, not HDR"
    // report). Without the halo texture (gl.a = 0) the whole boost applies as before.
    float boost_gain = mp.x;
    if (!sdr && gl.a > 0.0)
    {
        float mr = max(max(rgb.r, rgb.g), rgb.b);
        float source_share = saturate((mr - 1.35 * gl.a - 0.25) / 0.6);
        boost_gain = 1.0 + (mp.x - 1.0) * lerp(0.6, 1.0, source_share); // 0.6 = datapath::SURFACE_SHARE (was 0.2, then 0.3)
    }
    float3 boosted = sdr ? rgb : expand(rgb, boost_gain);
    if (!sdr)
    {
        float mb = max(max(boosted.r, boosted.g), boosted.b);
        float Yb = dot(float3(0.2126, 0.7152, 0.0722), boosted);
        boosted = lerp(boosted, float3(Yb, Yb, Yb), min(saturate((mb - 2.0) / (mb + 2.0)), 0.4));
    }

    float3 o = sqrt(boosted) + d * 0.003922; // gamma 2.0 encode; negative -> NaN
    o = sdr ? saturate(o) : max(o, 0.0);     // lower bound only in HDR (NaN -> 0)

    if (mp.w > 0.5 && mp.w < 1.5) // debug view
    {
        float mb = max(max(boosted.r, boosted.g), boosted.b);
        float mr = max(max(rgb.r, rgb.g), rgb.b);
        if (mb > mr * 1.02)
            o = float3(1.0, 0.0, 0.0);
        else if (occ > 0.1)
            o = float3(0.0, 0.0, 1.0);
        if (mp2.x > 0.0)
        {
            float3 hs = halo_source(scene_enc * scene_enc, max(gl.a, 0.0));
            if (max(max(hs.r, hs.g), hs.b) > 0.02)
                o = float3(0.0, 1.0, 0.0);
        }
        if (max(max(rf.r, rf.g), rf.b) > 0.002) // reflected pixels: magenta tint
            o = lerp(o, float3(1.0, 0.0, 1.0), 0.5);
    }
    if (mp.w > 2.5 && mp.w < 3.5) // debug materials: tint by the class of the RAW alpha (the same thresholds as 4340ca88)
    {
        float3 tint = c.w > 0.491961 ? float3(1.0, 0.0, 0.0) : (c.w > 0.240980 ? float3(0.0, 1.0, 0.0) : float3(0.0, 0.0, 1.0));
        o = lerp(o, tint, 0.55);
    }
    if (mp.w > 3.5 && mp.w < 4.5) // debug sun shafts (shafts.rs): sky cyan (alpha of t9 = sky share), shaft light yellow
    {
        float4 shd = shafts.SampleLevel(s0, i.uv.xy, 0);
        o = lerp(o, float3(0.0, 1.0, 1.0), 0.6 * saturate(shd.a));
        float shl = dot(float3(0.2126, 0.7152, 0.0722), max(shd.rgb, 0.0)) * hp.x;
        o = lerp(o, float3(1.0, 1.0, 0.0), saturate(shl * 6.0));
    }
    if (mp.w > 4.5 && mp.w < 5.5) // debug reflectivity: heat map of the SSR score (alpha of t8 when ssr.rs asks for it)
    {
        float h = saturate(rfs.a);
        // blue 0 -> cyan -> green -> yellow -> red 1; pixels that are not floors (alpha 0) show the dimmed scene only
        float3 heat = float3(saturate(4.0 * h - 2.0), saturate(min(4.0 * h, 4.0 - 4.0 * h)), saturate(2.0 - 4.0 * h));
        o = o * 0.3;
        if (rfs.a > 0.001)
            o = lerp(o, heat, 0.75);
    }
    if (fview_on > 0.5) // fur_view 1..4 (docs/fur-look.md): the diagnostic colour of the fur mask pixels is the final output
        o = fview_rgb;
    if (mp2.w > 0.5) // fur selector debug: the pixels of the fur mask turn magenta (what fur_ps / fur_vs / fur_min_vcb picked)
        o = lerp(o, float3(1.0, 0.0, 1.0), 0.8 * fur_mask_at(i.uv.xy, int2(0, 0)));
    if (shp.w > 0.5 && shp.w < 1.5 && shell_h >= 0.0) // shell debug: the shell pixels, red at the root (and the base layer) .. yellow at the tip
        o = lerp(o, float3(1.0, shell_h, 0.0), 0.8);
    if (shp.w > 1.5) // shell_debug=2: a colour per fur draw (furprobe.rs)
        o = shell_debug_draws(o, i.uv.xy, shell_key, shell_h);
    if (rp.w > 0.5) // rim selector debug: the pixels of the rim mask turn cyan (what rim_ps / rim_min_vcb picked)
        o = lerp(o, float3(0.0, 1.0, 1.0), 0.8 * rim_mask_at(i.uv.xy, int2(0, 0)));

    return float4(o, saturate(c.w));
}
