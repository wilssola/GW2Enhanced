// Fog colour and reference depth estimate for the AO release (see docs/m2-ao.md, "Fog and water release").
// Everything is estimated from the picture and only depth RATIOS are used, so the unit of the depth does not matter.
//
// Four draws (one triangle each, no input layout), all feeding a tiny state that stays on the GPU:
//   ps_ztile    linear depth -> 16x16 tiles: x = max valid depth, y = number of samples
//   ps_zreduce  tiles -> 1x1: x = max valid depth of the frame (zref)
//   ps_ftile    scene colour + depth + zref -> 16x16 tiles: rgb = sum of the encoded colour of the FAR pixels, a = their
//               count. A pixel is far when its depth is invalid (sky) or >= c0.x * zref
//   ps_stile    scene colour + depth -> 16x16 tiles: rgb = sum of the encoded colour of the SKY pixels (invalid depth or the
//               far plane, depth >= c3.x), a = their count
//   ps_fstate   tiles + previous state -> the 3x1 state (viewport 3x1, ping-pong, blended with the previous one):
//               texel 0 = (fog colour F rgb, confidence), texel 1 = (zref, far fraction, haze age in frames, haze source),
//               texel 2 = (haze colour H rgb, haze confidence): the colour of the aerial perspective (aerial.rs)
//               Confidence 0 means F is unknown (fewer than c0.y of the pixels are far): the AO then keeps only the
//               blue-water rule of the light-adjust shader.
//
// The haze colour H is NOT F: F is the colour of the farthest pixels of THIS frame, which with no sky in view (camera tilted
// down) is the far geometry itself, so a haze toward F is invisible. H is the colour of the horizon band of the sky (per tile
// column the lowest tile row holding sky), held when no sky is in view (source 2, never fading: stable whatever the pitch),
// drifting very slowly toward a neutral fallback built from F's luminance; with no sky ever seen it is that fallback (source 3,
// lower confidence). Source 1 = sky this frame (hysteresis: enter at c2.x of the pixels, stay down to c2.y), 0 = unknown.
//
// b0: c0 = x far ratio (0.9), y least far fraction (0.005), z confidence rise, w confidence fall
//     c1 = x follow rate of F, y follow rate of zref
//     c2 = x sky fraction to (re)enter source 1, y sky fraction to stay in it, z follow rate of H, w drift rate of a held H
//     c3 = x sky depth (0.98), y fallback confidence, z least sky share of a tile for the horizon band
// Slots: t0 scene colour (the engine's own SRV), t10 depth, t11 depth tiles, t12 zref, t13 colour tiles, t14 previous state,
// t15 sky tiles.

cbuffer params : register(b0)
{
    float4 c0;
    float4 c1;
    float4 c2;
    float4 c3;
};

Texture2D<float4> t_scene : register(t0);
Texture2D<float> t_depth : register(t10);
Texture2D<float4> t_ztile : register(t11);
Texture2D<float4> t_zstat : register(t12);
Texture2D<float4> t_ftile : register(t13);
Texture2D<float4> t_prev : register(t14);
Texture2D<float4> t_stile : register(t15);

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

// Invalid depth (<= 0, NaN, inf, huge) = sky or a cleared area, the same rule as the AO.
bool valid(float z)
{
    return z > 1e-5 && isfinite(z) && z < 1e30;
}

float4 ps_ztile(float4 sv : SV_POSITION) : SV_Target
{
    uint dw, dh;
    t_depth.GetDimensions(dw, dh);
    uint2 tile = uint2(sv.xy);
    uint x0 = tile.x * dw / 16, x1 = (tile.x + 1) * dw / 16;
    uint y0 = tile.y * dh / 16, y1 = (tile.y + 1) * dh / 16;
    float mx = 0.0, n = 0.0;
    [loop] for (uint y = y0; y < y1; y += 2)
    {
        [loop] for (uint x = x0; x < x1; x += 2)
        {
            float z = t_depth.Load(int3(x, y, 0));
            if (valid(z))
                mx = max(mx, z);
            n += 1.0;
        }
    }
    return float4(mx, n, 0.0, 0.0);
}

float4 ps_zreduce(float4 sv : SV_POSITION) : SV_Target
{
    float mx = 0.0;
    [loop] for (int y = 0; y < 16; y++)
    {
        [loop] for (int x = 0; x < 16; x++)
            mx = max(mx, t_ztile.Load(int3(x, y, 0)).x);
    }
    return float4(mx, 0.0, 0.0, 0.0);
}

float4 ps_ftile(float4 sv : SV_POSITION) : SV_Target
{
    uint dw, dh, sw, sh;
    t_depth.GetDimensions(dw, dh);
    t_scene.GetDimensions(sw, sh);
    uint2 tile = uint2(sv.xy);
    uint x0 = tile.x * dw / 16, x1 = (tile.x + 1) * dw / 16;
    uint y0 = tile.y * dh / 16, y1 = (tile.y + 1) * dh / 16;
    float far_z = t_zstat.Load(int3(0, 0, 0)).x * c0.x;
    float3 sum = float3(0.0, 0.0, 0.0);
    float cnt = 0.0;
    [loop] for (uint y = y0; y < y1; y += 2)
    {
        [loop] for (uint x = x0; x < x1; x += 2)
        {
            float z = t_depth.Load(int3(x, y, 0));
            if (!valid(z) || z >= far_z)
            {
                float2 uv = (float2(x, y) + 0.5) / float2(dw, dh);
                sum += min(max(t_scene.Load(int3(int2(uv * float2(sw, sh)), 0)).rgb, 0.0), 2.0);
                cnt += 1.0;
            }
        }
    }
    return float4(sum, cnt);
}

// Sky pixels (invalid depth, or the far plane the engine writes for the sky) per tile: the source of the haze colour H.
float4 ps_stile(float4 sv : SV_POSITION) : SV_Target
{
    uint dw, dh, sw, sh;
    t_depth.GetDimensions(dw, dh);
    t_scene.GetDimensions(sw, sh);
    uint2 tile = uint2(sv.xy);
    uint x0 = tile.x * dw / 16, x1 = (tile.x + 1) * dw / 16;
    uint y0 = tile.y * dh / 16, y1 = (tile.y + 1) * dh / 16;
    float3 sum = float3(0.0, 0.0, 0.0);
    float cnt = 0.0;
    [loop] for (uint y = y0; y < y1; y += 2)
    {
        [loop] for (uint x = x0; x < x1; x += 2)
        {
            float z = t_depth.Load(int3(x, y, 0));
            if (!valid(z) || z >= c3.x)
            {
                float2 uv = (float2(x, y) + 0.5) / float2(dw, dh);
                sum += min(max(t_scene.Load(int3(int2(uv * float2(sw, sh)), 0)).rgb, 0.0), 2.0);
                cnt += 1.0;
            }
        }
    }
    return float4(sum, cnt);
}

// The new fog colour state (texel 0) from the far-pixel measurement.
float4 next_fog(float4 p0, bool ok, float3 meas)
{
    float pc = p0.w;
    float conf = pc;
    float3 F = p0.rgb;
    if (ok)
    {
        // Nothing known before: start at the measurement; afterwards follow it smoothly.
        conf = (pc < 0.02) ? 1.0 : lerp(pc, 1.0, c0.z);
        F = (pc < 0.02) ? meas : lerp(p0.rgb, meas, c1.x);
    }
    else
    {
        conf = lerp(pc, 0.0, c0.w);
    }
    if (conf < 0.01)
        conf = 0.0;
    return float4(F, conf);
}

// A neutral, slightly cool and light haze guessed from the fog colour F when no sky has been seen (encoded colour).
float3 fallback_haze(float3 F)
{
    float g = dot(F, float3(0.2126, 0.7152, 0.0722));
    return saturate(g * 1.1 + 0.05) * float3(0.92, 0.97, 1.06);
}

// The new haze state: texel 2 (H rgb, confidence) in `h`, the age (frames since the sky was last measured) and the source
// (0 unknown, 1 sky, 2 held, 3 fallback) in `age_src`.
void next_haze(float4 p1, float4 p2, float4 fog, float3 hsum, float hcnt, float sky_frac, out float4 h, out float2 age_src)
{
    float src = p1.w;
    float age = p1.z;
    bool was_sky = src > 0.5 && src < 1.5;
    bool sky_ok = hcnt > 0.0 && sky_frac >= (was_sky ? c2.y : c2.x);
    bool fog_known = fog.w > 0.0;
    float3 fb = fallback_haze(fog.rgb);
    h = p2;
    if (sky_ok)
    {
        float3 meas = hsum / max(hcnt, 1.0);
        h.rgb = (p2.w < 0.02) ? meas : lerp(p2.rgb, meas, c2.z);
        h.w = (p2.w < 0.02) ? 1.0 : lerp(p2.w, 1.0, c2.z);
        src = 1.0;
        age = 0.0;
    }
    else if (p2.w > 0.0 && (was_sky || (src > 1.5 && src < 2.5)))
    {
        // Held: the last sky colour, never fading (no flicker when the camera tilts down); a very slow drift toward the
        // fallback so a new map seen without sky eventually adapts.
        if (fog_known)
            h.rgb = lerp(p2.rgb, fb, c2.w);
        src = 2.0;
        age = min(age + 1.0, 1e7);
    }
    else if (fog_known)
    {
        h.rgb = (p2.w < 0.02) ? fb : lerp(p2.rgb, fb, c2.z);
        h.w = (p2.w < 0.02) ? c3.y : lerp(p2.w, c3.y, c2.z);
        src = 3.0;
        age = min(age + 1.0, 1e7);
    }
    else
    {
        h.w = lerp(p2.w, 0.0, c0.w);
        if (h.w < 0.01)
        {
            h.w = 0.0;
            src = 0.0;
        }
        age = min(age + 1.0, 1e7);
    }
    age_src = float2(age, src);
}

float4 ps_fstate(float4 sv : SV_POSITION) : SV_Target
{
    float3 sum = float3(0.0, 0.0, 0.0);
    float far = 0.0, n = 0.0, sky = 0.0;
    float3 hsum = float3(0.0, 0.0, 0.0);
    float hcnt = 0.0;
    [loop] for (int x = 0; x < 16; x++)
    {
        bool band = false;
        // From the bottom of the picture up: the first tile row of this column that holds sky is the horizon band.
        [loop] for (int y = 15; y >= 0; y--)
        {
            float4 f = t_ftile.Load(int3(x, y, 0));
            float tn = t_ztile.Load(int3(x, y, 0)).y;
            float4 s = t_stile.Load(int3(x, y, 0));
            sum += f.rgb;
            far += f.a;
            n += tn;
            sky += s.a;
            if (!band && s.a > 0.0 && s.a >= c3.z * tn)
            {
                band = true;
                hsum += s.rgb;
                hcnt += s.a;
            }
        }
    }
    float frac_far = far / max(n, 1.0);
    float sky_frac = sky / max(n, 1.0);
    bool ok = far > 0.0 && frac_far >= c0.y;
    float3 meas = sum / max(far, 1.0);
    float zmax = t_zstat.Load(int3(0, 0, 0)).x;
    float4 p0 = t_prev.Load(int3(0, 0, 0));
    float4 p1 = t_prev.Load(int3(1, 0, 0));
    float4 p2 = t_prev.Load(int3(2, 0, 0));
    float4 fog = next_fog(p0, ok, meas);
    if (sv.x < 1.0)
        return fog;
    float4 h;
    float2 age_src;
    next_haze(p1, p2, fog, hsum, hcnt, sky_frac, h, age_src);
    if (sv.x >= 2.0)
        return h;
    float z = p1.x;
    if (zmax > 0.0)
        z = (z <= 0.0) ? zmax : lerp(z, zmax, c1.y);
    return float4(z, frac_far, age_src.x, age_src.y);
}
