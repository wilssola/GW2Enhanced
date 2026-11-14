// Screen-space sun shafts (god rays), see docs/shafts.md. Everything is estimated from the picture: the
// engine's shadow matrices and sun direction are not known.
//
// Six draws (one triangle each, no input layout), all at about 1/4 of the scene size except the reductions:
//   ps_dtile     linear depth -> 16x16 tiles: max valid depth, count of invalid samples, count of samples
//   ps_dreduce   tiles -> 1x1 frame depth statistics: x = max valid depth, y = fraction of invalid samples
//   ps_mask      scene colour + linear depth + depth statistics -> emitter (bright SKY colour; geometry = occluder =
//                black). A tap is sky when its depth is invalid, OR (when fewer than c3.y of the samples are invalid,
//                i.e. the sky is not encoded as invalid depth) when depth >= c3.x * the frame's max depth: a finite
//                far plane. rgb = emitted linear light, a = fraction of sky in the quarter pixel
//   ps_reduce_a  emitter -> 16x16 tiles of weighted sums (3 render targets)
//   ps_reduce_b  tiles -> the 1x1 sun state (2 render targets: position + confidence, colour tint), blended
//                with the previous state (ping-pong) so the sun does not jump and appears/disappears smoothly
//   ps_shafts    radial blur of the emitter towards the sun (GPU Gems 3 ch. 13), tinted by the sun colour
//
// b0: c0 = emitter size (w, h, 1/w, 1/h)
//     c1 = x ray length (fraction of the way to the sun), y decay per sample, z debug (> 0.5: cross at the sun)
//     c2 = x number of emitter pixels, y confidence rise, z confidence fall, w position/colour follow rate
//     c3 = x far-plane ratio (0.98), y largest invalid fraction for which the far-plane rule applies (0.02)
// Slots: t0 scene colour (the engine's own SRV), t10 linear depth, t11 emitter, t12..t14 tile sums,
//        t15/t16 sun state (previous for ps_reduce_b, current for ps_shafts), t17 depth statistics,
//        t18 depth tiles, s15 linear clamp sampler.
// Sun state: s0 = (u, v, confidence, sky fraction), s1 = (tint rgb, max valid depth of the frame).
// The shaft texture alpha is the sky share of the pixel (debug level 4 of the light-adjust shader colours it).

cbuffer params : register(b0)
{
    float4 c0;
    float4 c1;
    float4 c2;
    float4 c3;
};

Texture2D<float4> t_scene : register(t0);
Texture2D<float> t_depth : register(t10);
Texture2D<float4> t_emit : register(t11);
Texture2D<float4> t_ra : register(t12);
Texture2D<float4> t_rb : register(t13);
Texture2D<float4> t_rc : register(t14);
Texture2D<float4> t_sun0 : register(t15);
Texture2D<float4> t_sun1 : register(t16);
Texture2D<float4> t_dstat : register(t17);
Texture2D<float4> t_dtile : register(t18);
SamplerState smp : register(s15);

static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);
static const int TAPS = 4;       // 4x4 taps per quarter pixel in the mask pass
static const int NS = 56;        // samples per ray
static const float GAIN = 1.5;   // shaft light per unit of emitter light before the strength slider
static const float MAX_EMIT = 8.0;

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

// Depth tiles: the depth texture is sampled on a 2-pixel stride (plenty for a maximum and a fraction).
float4 ps_dtile(float4 sv : SV_POSITION) : SV_Target
{
    uint dw, dh;
    t_depth.GetDimensions(dw, dh);
    uint2 tile = uint2(sv.xy);
    uint x0 = tile.x * dw / 16, x1 = (tile.x + 1) * dw / 16;
    uint y0 = tile.y * dh / 16, y1 = (tile.y + 1) * dh / 16;
    float mx = 0.0, bad = 0.0, n = 0.0;
    [loop] for (uint y = y0; y < y1; y += 2)
    {
        [loop] for (uint x = x0; x < x1; x += 2)
        {
            float z = t_depth.Load(int3(x, y, 0));
            if (valid(z))
                mx = max(mx, z);
            else
                bad += 1.0;
            n += 1.0;
        }
    }
    return float4(mx, bad, n, 0.0);
}

float4 ps_dreduce(float4 sv : SV_POSITION) : SV_Target
{
    float mx = 0.0, bad = 0.0, n = 0.0;
    [loop] for (int y = 0; y < 16; y++)
    {
        [loop] for (int x = 0; x < 16; x++)
        {
            float4 t = t_dtile.Load(int3(x, y, 0));
            mx = max(mx, t.x);
            bad += t.y;
            n += t.z;
        }
    }
    return float4(mx, bad / max(n, 1.0), n, 0.0);
}

float4 ps_mask(float4 sv : SV_POSITION) : SV_Target
{
    uint sw, sh, dw, dh;
    t_scene.GetDimensions(sw, sh);
    t_depth.GetDimensions(dw, dh);
    float2 uv0 = floor(sv.xy) * c0.zw;
    float3 acc = float3(0.0, 0.0, 0.0);
    float sky = 0.0;
    float4 ds = t_dstat.Load(int3(0, 0, 0));
    bool far_rule = ds.x > 0.0 && ds.y < c3.y;
    float far_z = ds.x * c3.x;
    [loop] for (int j = 0; j < TAPS; j++)
    {
        [loop] for (int i = 0; i < TAPS; i++)
        {
            float2 uv = uv0 + (float2(i, j) + 0.5) / TAPS * c0.zw;
            float z = t_depth.Load(int3(int2(uv * float2(dw, dh)), 0));
            if (!valid(z) || (far_rule && z >= far_z))
            {
                float3 c = max(t_scene.Load(int3(int2(uv * float2(sw, sh)), 0)).rgb, 0.0);
                float3 L = min(c * c, MAX_EMIT); // gamma 2.0 decode
                float lum = dot(L, LUMA);
                acc += L * smoothstep(0.25, 0.7, lum); // only bright sky emits
                sky += 1.0;
            }
        }
    }
    float n = TAPS * TAPS;
    return float4(acc / n, sky / n);
}

struct RA
{
    float4 a : SV_Target0; // sum w*u, w*v, w, w*(u^2+v^2)
    float4 b : SV_Target1; // sum w*rgb, sum lum
    float4 c : SV_Target2; // sum sky fraction
};

RA ps_reduce_a(float4 sv : SV_POSITION)
{
    uint ew, eh;
    t_emit.GetDimensions(ew, eh);
    uint2 tile = uint2(sv.xy);
    uint x0 = tile.x * ew / 16, x1 = (tile.x + 1) * ew / 16;
    uint y0 = tile.y * eh / 16, y1 = (tile.y + 1) * eh / 16;
    float4 a = 0, b = 0, c = 0;
    [loop] for (uint y = y0; y < y1; y++)
    {
        [loop] for (uint x = x0; x < x1; x++)
        {
            float4 e = t_emit.Load(int3(x, y, 0));
            float2 p = (float2(x, y) + 0.5) / float2(ew, eh);
            float lum = dot(e.rgb, LUMA);
            float w = lum * lum * lum; // the sun core dominates the centroid
            a += float4(w * p, w, w * dot(p, p));
            b += float4(w * e.rgb, lum);
            c.x += e.a;
        }
    }
    RA o;
    o.a = a;
    o.b = b;
    o.c = c;
    return o;
}

struct RB
{
    float4 s0 : SV_Target0; // u, v, confidence, sky fraction
    float4 s1 : SV_Target1; // tint rgb (unit luminance), max valid depth of the frame
};

RB ps_reduce_b(float4 sv : SV_POSITION)
{
    float4 a = 0, b = 0, c = 0;
    [loop] for (int y = 0; y < 16; y++)
    {
        [loop] for (int x = 0; x < 16; x++)
        {
            a += t_ra.Load(int3(x, y, 0));
            b += t_rb.Load(int3(x, y, 0));
            c += t_rc.Load(int3(x, y, 0));
        }
    }
    float npix = max(c2.x, 1.0);
    float W = a.z;
    float2 pos = float2(0.5, 0.5);
    float3 col = float3(1.0, 1.0, 1.0);
    float spread = 1.0;
    if (W > 1e-9)
    {
        pos = a.xy / W;
        col = b.rgb / W;
        spread = sqrt(max(a.w / W - dot(pos, pos), 0.0));
    }
    float meanlum = b.w / npix;
    float skyfrac = c.x / npix;
    // Confidence of this frame's measurement: enough bright sky, concentrated rather than a uniform glow,
    // a believable sky share (almost all "sky" means the depth is unusable), not stuck at the very edge
    // (the real sun is then outside the picture and its direction is a guess).
    float conf_t = smoothstep(0.0005, 0.01, meanlum) * (1.0 - smoothstep(0.3, 0.6, spread)) * (1.0 - smoothstep(0.85, 0.97, skyfrac));
    float edge = max(abs(pos.x * 2.0 - 1.0), abs(pos.y * 2.0 - 1.0));
    conf_t *= 1.0 - 0.5 * smoothstep(0.9, 1.0, edge);
    if (W <= 1e-9)
        conf_t = 0.0;

    float4 p0 = t_sun0.Load(int3(0, 0, 0));
    float4 p1 = t_sun1.Load(int3(0, 0, 0));
    float pc = p0.z;
    float conf = lerp(pc, conf_t, conf_t > pc ? c2.y : c2.z);
    if (conf < 0.004)
        conf = 0.0;

    float lumc = max(dot(col, LUMA), 1e-4);
    float3 tint = clamp(col / lumc, 0.0, 3.0);
    float2 outpos = p0.xy;
    float3 outtint = p1.rgb;
    if (conf_t > 0.01)
    {
        if (pc < 0.01) // nothing before: start at the measurement
        {
            outpos = pos;
            outtint = tint;
        }
        else
        {
            outpos = lerp(p0.xy, pos, c2.w);
            outtint = lerp(p1.rgb, tint, c2.w);
        }
    }
    else if (pc < 0.01)
    {
        outtint = float3(1.0, 1.0, 1.0);
    }
    RB o;
    o.s0 = float4(outpos, conf, skyfrac);
    o.s1 = float4(outtint, t_dstat.Load(int3(0, 0, 0)).x);
    return o;
}

float ign(float2 p)
{
    return frac(52.9829189 * frac(dot(p, float2(0.06711056, 0.00583715))));
}

float4 ps_shafts(float4 sv : SV_POSITION) : SV_Target
{
    float4 s0 = t_sun0.Load(int3(0, 0, 0));
    float4 s1 = t_sun1.Load(int3(0, 0, 0));
    float conf = s0.z;
    float2 uv = sv.xy * c0.zw;
    float skyshare = t_emit.SampleLevel(smp, uv, 0).a; // alpha: sky share, for the debug view only
    if (conf < 0.002)
        return float4(0.0, 0.0, 0.0, skyshare);
    float2 d = s0.xy - uv;
    float2 stepv = d * c1.x / NS;
    float2 p = uv + stepv * ign(sv.xy); // per-pixel jitter: no banding
    float illum = 1.0;
    float acc = 0.0;
    [loop] for (int i = 0; i < NS; i++)
    {
        p += stepv;
        acc += dot(t_emit.SampleLevel(smp, p, 0).rgb, LUMA) * illum;
        illum *= c1.y;
    }
    acc /= NS;
    float3 rgb = acc * GAIN * conf * s1.rgb;
    if (c1.z > 0.5)
    {
        float2 q = abs(sv.xy - s0.xy * float2(c0.x, c0.y));
        if ((q.x < 1.0 && q.y < 7.0) || (q.y < 1.0 && q.x < 7.0))
            rgb = float3(4.0, 0.0, 4.0);
    }
    return float4(rgb, skyshare);
}
