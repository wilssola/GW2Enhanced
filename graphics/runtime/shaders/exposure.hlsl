// Auto exposure (eye adaptation) and the base layer of the local tone mapping. See docs/exposure.md.
//
// Everything runs in the mod's own targets from the scene colour the light-adjust draw reads (t0, gamma ~2.0 encoded, so
// linear = c * c) BEFORE the exposure is applied (no feedback loop). One triangle per draw, no input layout, no read-back.
//
//   ps_lum        scene (+ depth) -> L0 at 1/4: x = mean luminance (linear) of the valid pixels of the 4x4 block, y = valid share.
//                 A pixel is invalid when the depth says sky (<= 0, NaN, inf, huge): the sky is ignored.
//   ps_down_lin   L0 -> L1 at 1/8: x = log2 of the mean luminance (valid weighted), y = valid share.
//   ps_down_log   L1 -> L2 at 1/16 (the analysis level): the same in the log domain.
//   ps_blur       L2 -> B1 (horizontal) and B1 -> B2 (vertical): edge-aware (bilateral) blur of the log luminance.
//                 out: x = blurred log2 luminance (the BASE layer), y = raw log2 luminance, z = valid share.
//   ps_hist       L2 -> 64x8 (8 partial rows, summed by ps_state): x = weight, y = weight * log2 luminance per bin (centre-weighted, sky excluded).
//   ps_state      histogram + previous state -> 1x1 (ping-pong): the exposure compensation in EV, smoothed in time.
//                 out: x = EV applied (smoothed), y = measured log2 luminance (log-average without the brightest 0.1 %),
//                 z = the EV the scene asks for (limited, before the smoothing), w = 1 (initialised).
//
// b0:  c0 = x target luminance (linear, 1 = SDR white), y EV range when darkening, z EV range when brightening, w strength
//      c1 = x time constant when the scene gets brighter (s), y when it gets darker (s), z frame time (s), w 1 = jump (no smoothing)
//      c2 = x centre weight (0..1), y SDR clamp (> 0.5), z share of the brightest weight left out, w 0
//      c3 = x blur range sigma (EV), y = blur step in texels along x, z along y, w 0
// Slots: t0 scene, t10 depth, t11 first input, t12 second input.

cbuffer params : register(b0)
{
    float4 c0;
    float4 c1;
    float4 c2;
    float4 c3;
};

Texture2D<float4> t_scene : register(t0);
Texture2D<float> t_depth : register(t10);
Texture2D<float4> t_a : register(t11);
Texture2D<float4> t_b : register(t12);

#define BINS 64
#define LOG_MIN (-10.0)
#define LOG_RANGE 16.0
#define BLUR_TAPS 6
#define HIST_PARTS 8 // the histogram is built by 8 rows of pixels, each over every 8th row of the analysis level (shorter serial loops)

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

bool valid_depth(float z)
{
    return z > 1e-5 && isfinite(z) && z < 1e30;
}

static const float3 LUMW = float3(0.2126, 0.7152, 0.0722);

float4 ps_lum(float4 sv : SV_POSITION) : SV_Target
{
    uint sw, sh, dw, dh;
    t_scene.GetDimensions(sw, sh);
    t_depth.GetDimensions(dw, dh);
    int2 base = int2(sv.xy) * 4;
    float cap = c2.y > 0.5 ? 1.0 : 16.0;
    float sum = 0.0, n = 0.0, total = 0.0;
    [loop] for (int j = 0; j < 4; j++)
    {
        [loop] for (int i = 0; i < 4; i++)
        {
            int2 p = base + int2(i, j);
            if (p.x >= (int)sw || p.y >= (int)sh)
                continue;
            total += 1.0;
            if (dw > 0 && dh > 0)
            {
                int2 dp = min(int2((float2(p) + 0.5) / float2(sw, sh) * float2(dw, dh)), int2(dw, dh) - 1);
                if (!valid_depth(t_depth.Load(int3(dp, 0))))
                    continue;
            }
            float3 c = t_scene.Load(int3(p, 0)).rgb;
            c = min(max(c, 0.0), cap); // max also turns NaN into 0
            sum += min(dot(LUMW, c * c), 64.0);
            n += 1.0;
        }
    }
    return float4(n > 0.0 ? sum / n : 0.0, n / max(total, 1.0), 0.0, 1.0);
}

float4 ps_down_lin(float4 sv : SV_POSITION) : SV_Target
{
    uint w, h;
    t_a.GetDimensions(w, h);
    int2 b = int2(sv.xy) * 2;
    float sum = 0.0, v = 0.0, cnt = 0.0;
    [unroll] for (int j = 0; j < 2; j++)
    {
        [unroll] for (int i = 0; i < 2; i++)
        {
            int2 p = b + int2(i, j);
            if (p.x >= (int)w || p.y >= (int)h)
                continue;
            float4 t = t_a.Load(int3(p, 0));
            sum += t.x * t.y;
            v += t.y;
            cnt += 1.0;
        }
    }
    float lum = v > 0.0 ? sum / v : 0.0;
    return float4(v > 0.0 ? log2(max(lum, 1e-4)) : 0.0, v / max(cnt, 1.0), 0.0, 1.0);
}

float4 ps_down_log(float4 sv : SV_POSITION) : SV_Target
{
    uint w, h;
    t_a.GetDimensions(w, h);
    int2 b = int2(sv.xy) * 2;
    float sum = 0.0, v = 0.0, cnt = 0.0;
    [unroll] for (int j = 0; j < 2; j++)
    {
        [unroll] for (int i = 0; i < 2; i++)
        {
            int2 p = b + int2(i, j);
            if (p.x >= (int)w || p.y >= (int)h)
                continue;
            float4 t = t_a.Load(int3(p, 0));
            sum += t.x * t.y;
            v += t.y;
            cnt += 1.0;
        }
    }
    return float4(v > 0.0 ? sum / v : 0.0, v / max(cnt, 1.0), 0.0, 1.0);
}

// Edge-aware blur along one axis: the weight of a tap is a gaussian of the distance times a gaussian of the log-luminance
// difference to the centre, so a step edge (a bright wall next to a dark one) is never averaged across: no halo.
// The first pass reads the analysis level (x = log2 luminance, y = valid), the second one the first pass' output
// (x = blurred, y = raw, z = valid): both keep the raw value in y.
float4 ps_blur(float4 sv : SV_POSITION) : SV_Target
{
    uint w, h;
    t_a.GetDimensions(w, h);
    int2 p0 = int2(sv.xy);
    int2 stepv = int2(c3.y, c3.z);
    float4 ce = t_a.Load(int3(p0, 0));
    bool second = c3.w > 0.5;
    float raw_c = second ? ce.y : ce.x;
    float valid_c = second ? ce.z : ce.y;
    float inv2s2 = 1.0 / (2.0 * max(c3.x, 0.05) * max(c3.x, 0.05));
    float sum = 0.0, wsum = 0.0;
    [unroll] for (int i = -BLUR_TAPS; i <= BLUR_TAPS; i++)
    {
        int2 p = clamp(p0 + stepv * i, int2(0, 0), int2(w, h) - 1);
        float4 t = t_a.Load(int3(p, 0));
        float raw = second ? t.y : t.x;
        float val = second ? t.z : t.y;
        float d = raw - raw_c;
        float ws = exp(-(float)(i * i) / (2.0 * 4.0 * 4.0));
        float wt = ws * val * exp(-d * d * inv2s2);
        sum += wt * t.x; // averaged: the raw value in the first pass, the first pass' blurred one in the second
        wsum += wt;
    }
    float base = (wsum > 1e-6 && valid_c > 0.0) ? sum / wsum : raw_c;
    return float4(base, raw_c, valid_c, 1.0);
}

float center_weight(float2 uv)
{
    float2 d = uv - 0.5;
    return (1.0 - c2.x) + c2.x * exp(-dot(d, d) / (2.0 * 0.3 * 0.3));
}

float4 ps_hist(float4 sv : SV_POSITION) : SV_Target
{
    int bin = (int)sv.x;
    uint part = (uint)sv.y;
    uint w, h;
    t_a.GetDimensions(w, h);
    float sw = 0.0, sl = 0.0;
    [loop] for (uint y = part; y < h; y += HIST_PARTS)
    {
        [loop] for (uint x = 0; x < w; x++)
        {
            float4 t = t_a.Load(int3(x, y, 0));
            if (!(t.y > 0.0))
                continue;
            int b = (int)clamp(floor((t.x - LOG_MIN) / LOG_RANGE * BINS), 0.0, BINS - 1.0);
            if (b != bin)
                continue;
            float wt = t.y * center_weight((float2(x, y) + 0.5) / float2(w, h));
            sw += wt;
            sl += wt * t.x;
        }
    }
    return float4(sw, sl, 0.0, 1.0);
}

// Smooth limiter: identity up to 60 % of the range r, then a tanh roll-off that reaches r at infinity. C1, monotonic.
float soft_limit(float x, float r)
{
    if (r <= 1e-4)
        return 0.0;
    float a = 0.6 * r;
    float ax = abs(x);
    if (ax <= a)
        return x;
    float s = x < 0.0 ? -1.0 : 1.0;
    return s * (a + (r - a) * tanh((ax - a) / (r - a)));
}

float4 ps_state(float4 sv : SV_POSITION) : SV_Target
{
    float4 prev = t_b.Load(int3(0, 0, 0));
    bool init = prev.w > 0.5;
    float W = 0.0;
    [loop] for (int i = 0; i < BINS; i++)
    {
        [unroll] for (int r = 0; r < HIST_PARTS; r++)
            W += t_a.Load(int3(i, r, 0)).x;
    }
    if (!(W > 1e-4))
        return init ? prev : float4(0.0, 0.0, 0.0, 1.0); // nothing to measure (all sky / black): hold
    // Leave out the brightest share of the weight (lamps, the sun, specular glints) from the top bin down.
    float reject = c2.z * W;
    float sw = 0.0, sl = 0.0;
    [loop] for (int b = BINS - 1; b >= 0; b--)
    {
        float2 bin = float2(0.0, 0.0);
        [unroll] for (int r = 0; r < HIST_PARTS; r++)
            bin += t_a.Load(int3(b, r, 0)).xy;
        float take = min(bin.x, reject);
        reject -= take;
        float keep = bin.x - take;
        if (bin.x > 0.0)
        {
            sw += keep;
            sl += bin.y * (keep / bin.x);
        }
    }
    float avg = sw > 1e-6 ? sl / sw : LOG_MIN;
    float raw = log2(max(c0.x, 1e-4)) - avg;
    float x = raw * c0.w;
    float ev = x > 0.0 ? soft_limit(x, c0.z) : soft_limit(x, c0.y);
    float tau = ev < prev.x ? c1.x : c1.y;
    float alpha = 1.0 - exp(-max(c1.z, 0.0) / max(tau, 1e-3));
    float smoothed = (init && c1.w < 0.5) ? prev.x + (ev - prev.x) * alpha : ev;
    return float4(smoothed, avg, ev, 1.0);
}
