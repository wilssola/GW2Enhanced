// SMAA 1x (spatial, quality "medium": 16 pixel search, no diagonals, no corner rounding) with HDR-aware edge detection and
// blending, then a CAS-style contrast adaptive sharpen. See docs/aa-aniso.md.
//
// The picture is the engine's encoded scene colour (about gamma 2.0) in float16: values above 1.0 are legal highlights.
// Everything that DECIDES (edges) or MIXES (blend, sharpen weights) works on a soft-compressed copy of the colour:
//   comp(c) = c * s(m),  m = max(r,g,b),  s = 1 for m <= 1, (2 - 1/m) / m above
// so m' = m below 1 (a plain SDR picture is processed exactly like plain SMAA) and m' -> 2 for huge values: a 20x highlight
// next to a 40x highlight is not an edge, and mixing a highlight with a dark pixel does not leave a bright rim. uncomp()
// is the exact inverse. Hue is preserved (a per-pixel scalar).
//
// Instead of SMAA's AreaTex/SearchTex, the area under the edge line is computed analytically (the same trapezoid
// geometry, orthogonal patterns only): see areas(). Slots: b0 constants, t10 colour, t11 edges, t12 weights, t13 blended.
//
// b0: g_size = (w, h, 1/w, 1/h); g_p = (edge threshold, sharpen strength, local contrast factor, 0).

cbuffer C : register(b0)
{
    float4 g_size;
    float4 g_p;
};

Texture2D<float4> t_color : register(t10);
Texture2D<float2> t_edges : register(t11);
Texture2D<float4> t_w : register(t12);
Texture2D<float4> t_blend : register(t13);

static const int MAXS = 16;

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

float3 comp(float3 c)
{
    float m = max(c.r, max(c.g, c.b));
    float s = (m <= 1.0) ? 1.0 : (2.0 - 1.0 / m) / m;
    return c * s;
}

float3 uncomp(float3 c)
{
    float m = max(c.r, max(c.g, c.b));
    if (m <= 1.0)
        return c;
    m = min(m, 1.99998);
    float mo = 1.0 / (2.0 - m);
    return c * (mo / m);
}

int2 cl(int2 p) { return clamp(p, int2(0, 0), int2(g_size.xy) - 1); }

float3 colour(int2 p) { return comp(sane(t_color.Load(int3(cl(p), 0)).rgb)); }

float lumc(int2 p) { return dot(colour(p), float3(0.2126, 0.7152, 0.0722)); }

// ---- pass 1: luma edges (SMAA's LumaEdgeDetection, local contrast adaptation included) -------------------------------

float2 ps_edges(float4 pos : SV_Position) : SV_Target
{
    int2 p = int2(pos.xy);
    float L = lumc(p);
    float Ll = lumc(p + int2(-1, 0));
    float Lt = lumc(p + int2(0, -1));
    float2 delta = abs(L - float2(Ll, Lt));
    float2 e = step(g_p.x, delta);
    if (e.x + e.y == 0.0)
        return float2(0.0, 0.0);
    float Lr = lumc(p + int2(1, 0));
    float Lb = lumc(p + int2(0, 1));
    float2 mx = max(delta, abs(L - float2(Lr, Lb)));
    float Lll = lumc(p + int2(-2, 0));
    float Ltt = lumc(p + int2(0, -2));
    mx = max(mx, abs(float2(Ll, Lt) - float2(Lll, Ltt)));
    float fd = max(mx.x, mx.y);
    e *= step(fd, g_p.z * delta);
    return e;
}

// ---- pass 2: blend weights ---------------------------------------------------------------------------------------

float2 edge_at(int2 p) { return t_edges.Load(int3(p, 0)).xy; }

// Offset at the end of an edge line: a vertical edge crossing on side A (up / left) lifts the true edge towards A (+0.5),
// on side B (down / right) lowers it (-0.5); both or none: the line just ends (0).
float off(float a, float b)
{
    return (a > 0.5 && b < 0.5) ? 0.5 : ((b > 0.5 && a < 0.5) ? -0.5 : 0.0);
}

// (positive area, negative area) of a linear piece from f0 to f1 over width w.
float2 seg(float f0, float f1, float w)
{
    if (f0 >= 0.0 && f1 >= 0.0)
        return float2(0.5 * (f0 + f1) * w, 0.0);
    if (f0 <= 0.0 && f1 <= 0.0)
        return float2(0.0, -0.5 * (f0 + f1) * w);
    float s = f0 / (f0 - f1);
    if (f0 > 0.0)
        return float2(0.5 * f0 * s * w, -0.5 * f1 * (1.0 - s) * w);
    return float2(0.5 * f1 * (1.0 - s) * w, -0.5 * f0 * s * w);
}

// The line of length L has offset o1 at its start and o2 at its end (linear; two ends on the same side, a U, fall to 0 in
// the middle). Returns (positive, negative) area of the cell [t0, t0 + 1]: the share of the pixel on side A that really
// belongs to side B, and the other way round.
float2 areas(float o1, float o2, float L, float t0)
{
    float m = 0.5 * L;
    float vm = (o1 * o2 > 0.0) ? 0.0 : 0.5 * (o1 + o2);
    float t1 = t0 + 1.0;
    float2 acc = float2(0.0, 0.0);
    if (t0 < m)
    {
        float a = min(t1, m);
        acc += seg(lerp(o1, vm, t0 / m), lerp(o1, vm, a / m), a - t0);
    }
    if (t1 > m)
    {
        float a = max(t0, m);
        acc += seg(lerp(vm, o2, (a - m) / (L - m)), lerp(vm, o2, (t1 - m) / (L - m)), t1 - a);
    }
    return acc;
}

// x: this pixel blends towards its top neighbour, y: the top neighbour blends towards this pixel,
// z: this pixel towards its left neighbour, w: the left neighbour towards this pixel.
float4 ps_weights(float4 pos : SV_Position) : SV_Target
{
    int2 p = int2(pos.xy);
    float2 e = edge_at(p);
    float4 w = float4(0.0, 0.0, 0.0, 0.0);

    if (e.y > 0.5)
    {
        // Horizontal edge on top of p: follow it left and right while it continues without a crossing edge.
        int d1 = 0;
        [loop] for (int i = 0; i < MAXS; i++)
        {
            int2 q = p - int2(d1, 0);
            if (edge_at(q).x > 0.5 || edge_at(q + int2(0, -1)).x > 0.5) break;
            if (edge_at(q + int2(-1, 0)).y < 0.5) break;
            d1++;
        }
        int d2 = 0;
        [loop] for (int j = 0; j < MAXS; j++)
        {
            int2 q = p + int2(d2, 0);
            if (edge_at(q + int2(1, 0)).x > 0.5 || edge_at(q + int2(1, -1)).x > 0.5) break;
            if (edge_at(q + int2(1, 0)).y < 0.5) break;
            d2++;
        }
        int2 ql = p - int2(d1, 0);
        int2 qr = p + int2(d2, 0);
        float o1 = off(edge_at(ql + int2(0, -1)).x, edge_at(ql).x);
        float o2 = off(edge_at(qr + int2(1, -1)).x, edge_at(qr + int2(1, 0)).x);
        float2 a = areas(o1, o2, float(d1 + d2 + 1), float(d1));
        w.x = a.y;
        w.y = a.x;
    }

    if (e.x > 0.5)
    {
        // Vertical edge left of p: the same, going up and down.
        int d1 = 0;
        [loop] for (int i = 0; i < MAXS; i++)
        {
            int2 q = p - int2(0, d1);
            if (edge_at(q).y > 0.5 || edge_at(q + int2(-1, 0)).y > 0.5) break;
            if (edge_at(q + int2(0, -1)).x < 0.5) break;
            d1++;
        }
        int d2 = 0;
        [loop] for (int j = 0; j < MAXS; j++)
        {
            int2 q = p + int2(0, d2);
            if (edge_at(q + int2(0, 1)).y > 0.5 || edge_at(q + int2(-1, 1)).y > 0.5) break;
            if (edge_at(q + int2(0, 1)).x < 0.5) break;
            d2++;
        }
        int2 qu = p - int2(0, d1);
        int2 qd = p + int2(0, d2);
        float o1 = off(edge_at(qu + int2(-1, 0)).y, edge_at(qu).y);
        float o2 = off(edge_at(qd + int2(-1, 1)).y, edge_at(qd + int2(0, 1)).y);
        float2 a = areas(o1, o2, float(d1 + d2 + 1), float(d1));
        w.z = a.y;
        w.w = a.x;
    }
    return w;
}

// ---- pass 3: neighbourhood blending (in the compressed domain) -------------------------------------------------------

float4 ps_blend(float4 pos : SV_Position) : SV_Target
{
    int2 p = int2(pos.xy);
    float4 c0 = t_color.Load(int3(p, 0));
    float4 wq = t_w.Load(int3(p, 0));
    float t = wq.x;
    float l = wq.z;
    float b = t_w.Load(int3(p + int2(0, 1), 0)).y;
    float r = t_w.Load(int3(p + int2(1, 0), 0)).w;
    if (t + l + b + r <= 0.0)
        return c0;
    bool horiz = max(l, r) > max(t, b);
    float w1 = horiz ? l : t;
    float w2 = horiz ? r : b;
    int2 o1 = horiz ? int2(-1, 0) : int2(0, -1);
    int2 o2 = horiz ? int2(1, 0) : int2(0, 1);
    float s = w1 + w2;
    if (s > 1.0)
    {
        w1 /= s;
        w2 /= s;
    }
    float3 res = comp(sane(c0.rgb)) * (1.0 - w1 - w2) + colour(p + o1) * w1 + colour(p + o2) * w2;
    return float4(uncomp(res), c0.a);
}

// ---- pass 4: contrast adaptive sharpening (CAS; the compressed colour halved to 0..1) -------------------------------

float3 cas_at(int2 p) { return 0.5 * comp(sane(t_blend.Load(int3(cl(p), 0)).rgb)); }

float4 ps_cas(float4 pos : SV_Position) : SV_Target
{
    int2 p = int2(pos.xy);
    float4 src = t_blend.Load(int3(p, 0));
    float3 a = cas_at(p + int2(-1, -1));
    float3 b = cas_at(p + int2(0, -1));
    float3 c = cas_at(p + int2(1, -1));
    float3 d = cas_at(p + int2(-1, 0));
    float3 e = cas_at(p);
    float3 f = cas_at(p + int2(1, 0));
    float3 g = cas_at(p + int2(-1, 1));
    float3 h = cas_at(p + int2(0, 1));
    float3 i = cas_at(p + int2(1, 1));

    float3 mn = min(min(min(d, e), min(f, b)), h);
    float3 mn2 = min(mn, min(min(a, c), min(g, i)));
    mn += mn2;
    float3 mx = max(max(max(d, e), max(f, b)), h);
    float3 mx2 = max(mx, max(max(a, c), max(g, i)));
    mx += mx2;

    float3 amp = sqrt(saturate(min(mn, 2.0 - mx) / max(mx, 1e-5)));
    float peak = -0.2 * sqrt(saturate(g_p.y));
    float3 wgt = amp * peak;
    float3 rcpw = 1.0 / (1.0 + 4.0 * wgt);
    // Dark side may undershoot freely (down to 0); the bright side may overshoot a little, never to the end of the
    // compressed range (which would decompress to a huge value).
    float3 mxc = max(max(max(b, d), max(e, f)), h);
    float3 res = clamp(((b + d + f + h) * wgt + e) * rcpw, 0.0, mxc + 0.2 * mxc * (1.0 - mxc));
    return float4(max(uncomp(res * 2.0), 0.0), src.a);
}
