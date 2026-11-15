// Shell fur (src/shellfur.rs, docs/shell-fur.md). The selected fur draws of the engine's opaque pass are recorded (their state and a
// copy of their vertex constant buffers) and drawn AGAIN at the start of the light-adjust draw, once the frame's depth is final, with
// the engine's own vertex shader, a geometry shader of the mod that extrudes every triangle N times along its normal (the shells), and
// the pixel shader below that keeps the strand pixels of each shell. The result is the mod's R32_FLOAT target `shell`: per pixel the
// packed strand fragment with the HIGHEST shell (MAX blend of the packed key, see ps_shell), 0 = no strand. light_adjust_hdr.hlsl reads
// it as t24 and paints the strands with the colour of the body at their root (the anchor).
//
// The GEOMETRY shaders are generated per vertex-shader output signature by shellfur.rs (their input struct must sit on the vertex
// shader's registers, like the fur mask's uv shader): the generated text defines SHELL_GS, the struct VIn, V_POS(j) (the clip position),
// V_UV(j) (the mesh uv), V_N(j) / HAS_N (an optional per-vertex normal) and input_sum(), then this file follows. Without SHELL_GS only
// the pixel shaders are compiled.
//
// Temporal stability (the "flicker" of the first version, docs/shell-fur.md): everything here is a pure function of the mesh, the uv
// and constants that do not jump between frames: ONE unit (engine units per metre) for the whole frame, measured by gs_measure and
// smoothed with hysteresis on the CPU (no per-triangle guess that drops triangles); the LOD shortens and fades the strands continuously
// (no shell count that jumps per triangle); the strand coverage is soft (box-filtered edge, a minimum width, the two density levels
// blended by coverage, soft tips) instead of a 0/1 test on sub-pixel strands; the wind phase comes from the CPU as sin / cos (no wrap).

// t0 = the engine's linear depth of this frame (R32_FLOAT, normalised: 1.0 = far plane = sky; the AO's capture, ao::depth_resource).
Texture2D<float> lin_depth : register(t0);

static const float SKY_DEPTH = 0.98;   // aerial::SKY_DEPTH: the sky / far plane
static const float MAX_SHELLS = 32.0;
static const float FACE_LO = 0.08;     // v8 (shellfur::FACE_LO / FACE_HI): no shells on a triangle whose |cos(normal, view ray)| is below
static const float FACE_HI = 0.20;     // FACE_LO (seen edge-on), full shells from FACE_HI
static const int FT_DIL = 3;           // v8 (shellfur::FT_DIL): the flat (horn) and glossy (claw) tests also look this many texels away

struct GOut
{
    float4 pos : SV_Position;
    float4 a : TEXCOORD0; // xy anchor (the root's pixel position), zw mesh uv
    float4 b : TEXCOORD1; // x h (0 root .. 1 tip), y the root's linear depth (normalised, like t0), z LOD fade (1 near .. 0), w the
                          // root's DEVICE depth (clip z / w: what the engine's depth buffer holds where the root is visible)
    float4 c : TEXCOORD2; // x this shell vertex's own linear depth (normalised: the strand fragment's depth, not the root's), y the
                          // root's view depth (engine units, the clip w), z this shell vertex's view depth (engine units), w v9: 1 =
                          // the triangle touches an excluded island (the strand's texel tests also look FT_NEAR texels away)
    nointerpolation float4 d : TEXCOORD3; // v8: xy the engine's device depth per screen pixel over the BASE triangle (its plane: the
                          // root test compares the engine's depth with it exactly), z extra root tolerance (a triangle too small on the
                          // screen for a plane: its depth range), w engine units per metre (0 = the device-depth fragment test is off
                          // for this triangle: its depth is not the 1 / w form of a standard projection)
};

// The uv-space footprint of a recorded draw (gs_uv -> ps_feat): each triangle drawn at its mesh uv, so every covered cell of a small
// target holds the features of the draw's own atlas texels (the per-draw coat statistics, ps_stats).
struct UOut
{
    float4 pos : SV_Position;
    float4 uv : TEXCOORD0; // xy mesh uv, zw unused
};

// The unit measurement (gs_measure -> ps_measure): one full-viewport triangle per measured mesh triangle into a 1x1 RGBA32F target
// blended with ADD: (log2 units per metre, its square, 1, 0). shellfur.rs reads the sum back (a few frames later, never waiting).
struct MOut
{
    float4 pos : SV_Position;
    float4 v : TEXCOORD0;
};

uint shell_hash(uint x, uint y, uint z)
{
    uint h = x * 374761393u + y * 668265263u + z * 2246822519u;
    h = (h ^ (h >> 13)) * 1274126177u;
    h ^= h >> 16;
    return h;
}

float shell_rand(uint2 c, uint salt)
{
    return (float)(shell_hash(c.x, c.y, salt) & 0xFFFFFFu) / 16777215.0;
}

float lin_at(float2 px)
{
    uint w, h;
    lin_depth.GetDimensions(w, h);
    if (w == 0 || h == 0)
        return 0.0;
    int2 p = clamp(int2(floor(px)), int2(0, 0), int2(w, h) - 1);
    return lin_depth.Load(int3(p, 0));
}

bool valid_depth(float z)
{
    return z > 1e-6 && z < SKY_DEPTH;
}

#ifdef SHELL_GS

// t1 of the geometry shader = the engine's final DEVICE depth (the depth buffer the fur draws used, a shader view of its typeless
// texture). It tells which vertices are visible, so the unit measurement below only uses the triangle's own pixels. Unbound (a depth
// format no shader can read) = every sample counts (an occluder in front can then fool the measurement of that triangle).
Texture2D<float> dev_depth : register(t1);

// v9 (furisland.rs, docs/shell-fur.md "v9"): t2 of the shell geometry shader = the per-triangle bits of the mesh's UV-island analysis,
// 2 bits per triangle (SV_PrimitiveID), 16 per word: bit 0 = the triangle belongs to an excluded island / sub-region (a horn, a claw,
// a fang: NO shell, no layer, wherever it projects), bit 1 = a fur triangle touching such a part (its strands' texel tests look
// farther). Unbound (the analysis pending / failed / off): nothing changes (the v8 texel rule alone).
Buffer<uint> tri_bits : register(t2);

// b0 of the geometry shaders (shellfur::gs_cb_values).
cbuffer shell_gs : register(b0)
{
    float4 g0; // x shells N (0 = none), y strand length in metres, z largest extrusion in px, w engine units per metre (0 = per triangle)
    float4 g1; // x P00, y P11 (the projection scale the view space is rebuilt with), z viewport width, w viewport height
    float4 g2; // x viewport x, y viewport y, z front face counter-clockwise (1) / clockwise (0), w LOD distance (m)
    float4 g3; // x wind, y gravity, z unused, w metres per 1.0 of the normalised linear depth (depth_unit)
    float4 g4; // the wind phases from the CPU: sin a, cos a, sin b, cos b (continuous, no wrap)
    float4 g5; // x v11: 1 = the SKIN pass (gs_shell emits only the base layer, h = 0, on every triangle whatever its distance or
               // angle: no LOD end, no edge-on fade; ps_skin), 0 = the shells; yzw unused
};

float2 to_px(float4 c)
{
    float2 ndc = c.xy / c.w;
    return float2((ndc.x * 0.5 + 0.5) * g1.z + g2.x, (0.5 - 0.5 * ndc.y) * g1.w + g2.y);
}

// Rotates v by the rotation that takes the unit vector a onto the unit vector b (shortest arc).
float3 rotate_onto(float3 v, float3 a, float3 b)
{
    float3 k = cross(a, b);
    float s = length(k);
    float c = dot(a, b);
    if (s < 1e-5)
        return c > 0.0 ? v : -v;
    k /= s;
    return v * c + cross(k, v) * s + k * dot(k, v) * (1.0 - c);
}

// Engine units per metre measured at this triangle: the ratio of the clip w to the linear depth (in metres) where the vertices and the
// centroid are visible; two samples within 3 % of each other decide. 0 = no agreement.
float measure_upm(float4 C[3])
{
    float4 k = float4(-1.0, -1.0, -1.0, -1.0);
    float4 cc = (C[0] + C[1] + C[2]) / 3.0;
    uint dw, dh;
    dev_depth.GetDimensions(dw, dh);
    float zmin = min(min(C[0].z / C[0].w, C[1].z / C[1].w), C[2].z / C[2].w);
    float zmax = max(max(C[0].z / C[0].w, C[1].z / C[1].w), C[2].z / C[2].w);
    float ztol = 2e-5 + 0.5 * (zmax - zmin); // D24 rounding, a depth bias, and the depth change within the triangle
    [unroll]
    for (int j = 0; j < 4; j++)
    {
        float4 c = j < 3 ? C[min(j, 2)] : cc;
        float2 px = to_px(c);
        float z = lin_at(px);
        bool mine = true; // the sample shows this triangle (not an occluder in front of it)
        if (dw > 0 && dh > 0)
        {
            float zd = dev_depth.Load(int3(clamp(int2(floor(px)), int2(0, 0), int2(dw, dh) - 1), 0));
            mine = abs(zd - c.z / c.w) <= ztol;
        }
        k[j] = mine && valid_depth(z) && g3.w > 0.0 ? c.w / (z * g3.w) : -1.0;
    }
    // The six pairs: the first that agrees within 3 % decides.
    const int2 PAIRS[6] = { int2(0, 3), int2(1, 3), int2(2, 3), int2(0, 1), int2(1, 2), int2(0, 2) };
    [unroll]
    for (int q = 0; q < 6; q++)
    {
        float a = k[PAIRS[q].x], b = k[PAIRS[q].y];
        if (a > 0.0 && b > 0.0 && abs(a - b) <= 0.03 * max(a, b))
            return 0.5 * (a + b);
    }
    return 0.0;
}

// The unit the shells use: the frame's unit from the CPU (shell_unit, or the smoothed measurement); 0 there = the per-triangle
// measurement (only the first frames, before the CPU has a measurement: such a triangle may drop out).
float units_per_metre(float4 C[3])
{
    if (g0.w > 0.0)
        return g0.w;
    return measure_upm(C);
}

bool load_tri(VIn v[3], out float4 C[3], out float3 P[3], out float2 U[3])
{
    bool behind = false;
    [unroll]
    for (int j = 0; j < 3; j++)
    {
        C[j] = V_POS(j);
        U[j] = V_UV(j);
        behind = behind || !(C[j].w > 1e-4); // behind the camera (or NaN)
        P[j] = float3(C[j].x / g1.x, C[j].y / g1.y, C[j].w); // view space rebuilt with the assumed projection scale
    }
    return !behind;
}

// The unit measurement: every triangle with an agreeing measurement adds (log2 upm, log2 upm squared, 1) to the 1x1 target.
[maxvertexcount(3)]
void gs_measure(triangle VIn v[3], inout TriangleStream<MOut> stream)
{
    float keep_in = 1.0 - 1e-6 * saturate(abs(input_sum(v)) * 1e-30); // every input is read (the signature keeps them all)
    float4 C[3];
    float3 P[3];
    float2 U[3];
    if (!load_tri(v, C, P, U))
        return;
    float upm = measure_upm(C);
    if (!(upm > 0.0) || !(upm < 1e9))
        return;
    float l = log2(upm) * keep_in;
    const float2 FULL[3] = { float2(-1.0, -1.0), float2(-1.0, 3.0), float2(3.0, -1.0) };
    [unroll]
    for (int j = 0; j < 3; j++)
    {
        MOut o;
        o.pos = float4(FULL[j], 0.5, 1.0);
        o.v = float4(l, l * l, 1.0, 0.0);
        stream.Append(o);
    }
}


// v17: a triangle with a vertex behind the camera used to be dropped WHOLE
// (`load_tri`), and in first person, with the camera inside or right at the character, many of its large triangles reach behind the
// camera: the fur nearest the eye vanished with the view angle. Now the triangle is clipped at the NEAR PLANE (clip z >= 0; and w >=
// SHELL_WCLIP; one or two sub-triangles, the uv and normals interpolated) and its visible part grows shells. (A clip at the eye
// itself, w > 0, projects the cut vertices far off screen and the root plane test then fails: measured 0.22 strand keys per px.)
static const float SHELL_WCLIP = 1e-3;

[instance(32)]
[maxvertexcount(6)]
void gs_shell(triangle VIn v[3], uint id : SV_GSInstanceID, uint pid : SV_PrimitiveID, inout TriangleStream<GOut> stream)
{
    float n_max = min(g0.x, MAX_SHELLS);
    // v11: the skin pass (g5.x = 1) draws the base layer only, on every triangle the island bits keep (ps_skin).
    bool skin_pass = g5.x > 0.5;
    if (skin_pass ? id != 0u : ((float)id >= n_max || n_max < 2.0))
        return;
    // v9: an excluded triangle grows no shell at all; a near one flags its fragments (o.c.w).
    float near_part = 0.0;
    uint nbits;
    tri_bits.GetDimensions(nbits);
    if (nbits > 0u)
    {
        uint tf = (tri_bits.Load((int)(pid >> 4u)) >> ((pid & 15u) * 2u)) & 3u;
        if (tf == 1u)
            return;
        near_part = tf == 2u ? 1.0 : (tf == 3u ? 2.0 : 0.0); // v15: 3 = a two-tone limb triangle (no colour tests)
    }
    float keep_in = 1.0 - 1e-6 * saturate(abs(input_sum(v)) * 1e-30); // every input is read (the signature keeps them all)
    float h = skin_pass ? 0.0 : (float)id / (n_max - 1.0); // instance 0 = the base layer (h = 0, on the surface), the last one = the tips
    float4 C0[3];
    float2 U0[3];
    float3 N0[3];
    float d[3];
    int nin = 0;
    [unroll]
    for (int j = 0; j < 3; j++)
    {
        C0[j] = V_POS(j);
        U0[j] = V_UV(j);
#if HAS_N
        N0[j] = V_N(j);
#else
        N0[j] = float3(0.0, 0.0, 1.0);
#endif
        d[j] = min(C0[j].z, C0[j].w - SHELL_WCLIP); // the near plane (z >= 0) and in front of the eye (NaN: counted as behind)
        nin += d[j] > 0.0 ? 1 : 0;
    }
    if (nin == 0)
        return;
    // v17: the triangle clipped at w = SHELL_WCLIP. Rotated (selects, no dynamic indexing) so that q0 is the special vertex: the one
    // behind when one is behind (the visible part is a quad: two sub-triangles), the one in front when two are behind (one sub-triangle).
    int r = 0;
    if (nin == 2)
        r = !(d[0] > 0.0) ? 0 : (!(d[1] > 0.0) ? 1 : 2);
    else if (nin == 1)
        r = d[0] > 0.0 ? 0 : (d[1] > 0.0 ? 1 : 2);
    float4 qc0 = r == 0 ? C0[0] : (r == 1 ? C0[1] : C0[2]);
    float4 qc1 = r == 0 ? C0[1] : (r == 1 ? C0[2] : C0[0]);
    float4 qc2 = r == 0 ? C0[2] : (r == 1 ? C0[0] : C0[1]);
    float2 qu0 = r == 0 ? U0[0] : (r == 1 ? U0[1] : U0[2]);
    float2 qu1 = r == 0 ? U0[1] : (r == 1 ? U0[2] : U0[0]);
    float2 qu2 = r == 0 ? U0[2] : (r == 1 ? U0[0] : U0[1]);
    float3 qn0 = r == 0 ? N0[0] : (r == 1 ? N0[1] : N0[2]);
    float3 qn1 = r == 0 ? N0[1] : (r == 1 ? N0[2] : N0[0]);
    float3 qn2 = r == 0 ? N0[2] : (r == 1 ? N0[0] : N0[1]);
    float qd0 = r == 0 ? d[0] : (r == 1 ? d[1] : d[2]);
    float qd1 = r == 0 ? d[1] : (r == 1 ? d[2] : d[0]);
    float qd2 = r == 0 ? d[2] : (r == 1 ? d[0] : d[1]);
    // the crossing points on the edges q0-q1 and q2-q0 (only used when q0 and its neighbours lie on different sides)
    float t01 = saturate(qd0 / ((qd0 - qd1) == 0.0 ? 1e-12 : (qd0 - qd1)));
    float t20 = saturate(qd2 / ((qd2 - qd0) == 0.0 ? 1e-12 : (qd2 - qd0)));
    float4 xc01 = lerp(qc0, qc1, t01), xc20 = lerp(qc2, qc0, t20);
    float2 xu01 = lerp(qu0, qu1, t01), xu20 = lerp(qu2, qu0, t20);
    float3 xn01 = lerp(qn0, qn1, t01), xn20 = lerp(qn2, qn0, t20);
    [unroll]
    for (int sub = 0; sub < 2; sub++)
    {
        if (sub == 1 && nin != 2)
            break;
        if (sub == 1)
            stream.RestartStrip();
        // nin 3: (q0, q1, q2); nin 2: (x01, q1, q2) then (x01, q2, x20); nin 1: (q0, x01, x20)
        float4 C[3];
        float2 U[3];
        float3 NV[3];
        if (nin == 3)
        {
            C[0] = qc0; C[1] = qc1; C[2] = qc2;
            U[0] = qu0; U[1] = qu1; U[2] = qu2;
            NV[0] = qn0; NV[1] = qn1; NV[2] = qn2;
        }
        else if (nin == 1)
        {
            C[0] = qc0; C[1] = xc01; C[2] = xc20;
            U[0] = qu0; U[1] = xu01; U[2] = xu20;
            NV[0] = qn0; NV[1] = xn01; NV[2] = xn20;
        }
        else if (sub == 0)
        {
            C[0] = xc01; C[1] = qc1; C[2] = qc2;
            U[0] = xu01; U[1] = qu1; U[2] = qu2;
            NV[0] = xn01; NV[1] = qn1; NV[2] = qn2;
        }
        else
        {
            C[0] = xc01; C[1] = qc2; C[2] = xc20;
            U[0] = xu01; U[1] = qu2; U[2] = xu20;
            NV[0] = xn01; NV[1] = qn2; NV[2] = xn20;
        }
        float3 P[3];
        [unroll]
        for (int jp = 0; jp < 3; jp++)
            P[jp] = float3(C[jp].x / g1.x, C[jp].y / g1.y, C[jp].w); // view space rebuilt with the assumed projection scale
        float3 fn = cross(P[1] - P[0], P[2] - P[0]);
        float fl = length(fn);
        if (!(fl > 1e-12))
            continue;
        fn /= fl;
        if (g2.z > 0.5)
            fn = -fn; // counter-clockwise front faces: the outward normal is the other way round
        float upm = units_per_metre(C);
        if (!(upm > 0.0))
            continue;
        // LOD: the strands get shorter and fainter with the distance (continuously: no shell count that jumps), none beyond the LOD
        // distance. The distance is the triangle's centroid's, with the frame's single unit: neighbouring triangles agree.
        float dist = (C[0].w + C[1].w + C[2].w) / (3.0 * upm);
        float lod = max(g2.w, 0.5);
        float keep = skin_pass ? 1.0 : 1.0 - smoothstep(0.5 * lod, lod, dist);
        if (!(keep > 0.0))
            continue;
        // v8: no shells on a triangle seen edge-on (a flat card / strip / hem at a grazing angle: its strands all stand parallel in one
        // plane and draw a hard-edged "comb" or a rectangle of strands; a curved coat's outline keeps the strands of its less grazing
        // triangles). Faded over FACE_LO..FACE_HI of |cos| between the face normal and the view ray.
        float3 cen = (P[0] + P[1] + P[2]) / 3.0;
        float facing = abs(dot(fn, cen)) / max(length(cen), 1e-6);
        float face = skin_pass ? 1.0 : smoothstep(FACE_LO, FACE_HI, facing);
        if (!(face > 0.0))
            continue;
        // v8: the base triangle's device-depth plane over the screen (exact: the engine's depth at a pixel centre where this triangle is
        // the visible surface is this plane + the draw's depth bias), and whether its depth has the 1 / w form of a standard projection
        // (C.w - C.z = the near plane's distance, the same at every vertex): only then the fragment test converts device depths to view
        // depths.
        float2 S0 = to_px(C[0]), S1 = to_px(C[1]), S2 = to_px(C[2]);
        float Z0 = C[0].z / C[0].w, Z1 = C[1].z / C[1].w, Z2 = C[2].z / C[2].w;
        float2 e1 = S1 - S0, e2 = S2 - S0;
        float det = e1.x * e2.y - e1.y * e2.x;
        float4 plane = float4(0.0, 0.0, 0.0, 0.0);
        if (abs(det) > 1e-3)
            plane.xy = float2((Z1 - Z0) * e2.y - (Z2 - Z0) * e1.y, (Z2 - Z0) * e1.x - (Z1 - Z0) * e2.x) / det;
        else
            plane.z = max(abs(Z1 - Z0), abs(Z2 - Z0));
        float n0 = C[0].w - C[0].z, n1 = C[1].w - C[1].z, n2 = C[2].w - C[2].z;
        float nlo = min(min(n0, n1), n2), nhi = max(max(n0, n1), n2);
        plane.w = (nlo > 0.0 && nhi <= 1.05 * nlo) ? upm : 0.0;
        if (!all(plane == plane))
            continue;
        float L = g0.y * upm * (0.35 + 0.65 * keep); // strand length in engine units
        // Per-vertex direction: the face normal, or the vertex normal turned into the rebuilt view space by the rotation that takes the
        // triangle's mean vertex normal onto its face normal (smooth shells, no cracks along creases; the face normal where it looks wrong).
        float3 D[3] = { fn, fn, fn };
        float3 nm = float3(0.0, 0.0, 0.0);
    #if HAS_N
        [unroll]
        for (int j2 = 0; j2 < 3; j2++)
            nm += NV[j2];
        float nml = length(nm);
    #endif
        [unroll]
        for (int j3 = 0; j3 < 3; j3++)
        {
            D[j3] = fn;
    #if HAS_N
            float3 vn = NV[j3];
            float vl = length(vn);
            if (nml > 1e-4 && vl > 0.5 && vl < 1.5)
            {
                float3 d = rotate_onto(vn / vl, nm / nml, fn);
                if (dot(d, fn) > 0.3)
                    D[j3] = normalize(d);
            }
    #endif
        }
        // Gravity and wind bend the strand: their share grows with h squared (the tips move, the roots stay). The wind is a slow sway whose
        // phase varies over the mesh uv: sin(a + u) from the CPU's sin a / cos a (exact, continuous in time).
        float bend = h * h;
        float3 down = float3(0.0, -1.0, 0.0);
        float3 offs[3] = { float3(0.0, 0.0, 0.0), float3(0.0, 0.0, 0.0), float3(0.0, 0.0, 0.0) };
        float max_px = 0.0;
        [unroll]
        for (int j4 = 0; j4 < 3; j4++)
        {
            float u = U[j4].x * 37.0 + U[j4].y * 23.0;
            float su = sin(u), cu = cos(u);
            float s1 = g4.x * cu + g4.y * su;              // sin(a + u)
            float s2 = g4.z * cu * 0.7 + g4.w * su * 0.7;  // ~ sin(b + 0.7 u), a second slower component
            float3 wind = float3(1.0, 0.15 * s2, 0.35) * (g3.x * s1);
            offs[j4] = (D[j4] * h + (down * g3.y + wind) * (0.5 * bend)) * L;
            // the screen length of the full strand (h = 1) at this vertex, for the clamp
            float3 tip = P[j4] + (D[j4] + (down * g3.y + wind) * 0.5) * L;
            float4 ct = float4(tip.x * g1.x, tip.y * g1.y, 0.0, max(tip.z, 1e-4));
            max_px = max(max_px, length(to_px(ct) - to_px(C[j4])));
        }
        float fac = max_px > g0.z ? g0.z / max_px : 1.0;
        float3 Q[3] = { P[0], P[1], P[2] };
        bool front = true;
        [unroll]
        for (int j5 = 0; j5 < 3; j5++)
        {
            Q[j5] = P[j5] + offs[j5] * fac;
            front = front && Q[j5].z > 1e-4;
        }
        if (!front)
            continue;
        [unroll]
        for (int j6 = 0; j6 < 3; j6++)
        {
            GOut o;
            float zn = C[j6].z / C[j6].w; // the root's device depth: the shells do not use the hardware depth test (see ps_shell)
            o.pos = float4(Q[j6].x * g1.x, Q[j6].y * g1.y, zn * Q[j6].z, Q[j6].z);
            o.a = float4(to_px(C[j6]), U[j6]);
            o.b = float4(h * keep_in, C[j6].w / (upm * g3.w), keep * face, zn); // y = the root's own linear depth (normalised), w its device depth
            o.c = float4(Q[j6].z / (upm * g3.w), C[j6].w, Q[j6].z, near_part); // the strand fragment's own linear depth (normalised); view depths; v9 near
            o.d = plane;
            stream.Append(o);
        }

    }
}

// The draw's uv-space footprint (ps_feat): every triangle at its mesh uv (x right, v down), no culling (the caller's rasterizer state).
// A triangle whose uv leaves 0..1 is clipped there (the atlas uv of the character draws is inside 0..1).
[maxvertexcount(3)]
void gs_uv(triangle VIn v[3], inout TriangleStream<UOut> stream)
{
    float keep_in = 1.0 - 1e-6 * saturate(abs(input_sum(v)) * 1e-30); // every input is read (the signature keeps them all)
    [unroll]
    for (int j = 0; j < 3; j++)
    {
        float2 u = V_UV(j);
        UOut o;
        o.pos = float4(u.x * 2.0 - 1.0, 1.0 - u.y * 2.0, 0.5, keep_in);
        o.uv = float4(u, 0.0, 0.0);
        stream.Append(o);
    }
}

// v9: the island capture (stream output only, no rasterized stream; furisland.rs reads it back): one point per triangle with its
// three clip positions (x, y, w), its primitive id (the order check) and its three ATLAS uv (what the shells and the strand texel
// tests see). 5 float4 = 80 bytes per triangle.
struct TOut
{
    float4 a : TEXCOORD0; // clip x y w of vertex 0, the primitive id
    float4 b : TEXCOORD1; // clip x y w of vertex 1, 0
    float4 c : TEXCOORD2; // clip x y w of vertex 2, 0
    float4 d : TEXCOORD3; // uv 0, uv 1
    float4 e : TEXCOORD4; // uv 2, 0, 1
};

[maxvertexcount(1)]
void gs_tri(triangle VIn v[3], uint pid : SV_PrimitiveID, inout PointStream<TOut> stream)
{
    float keep_in = 1.0 - 1e-6 * saturate(abs(input_sum(v)) * 1e-30); // every input is read (the signature keeps them all)
    float4 C0 = V_POS(0), C1 = V_POS(1), C2 = V_POS(2);
    TOut o;
    o.a = float4(C0.x, C0.y, C0.w, (float)pid);
    o.b = float4(C1.x, C1.y, C1.w, 0.0);
    o.c = float4(C2.x, C2.y, C2.w, 0.0);
    o.d = float4(V_UV(0), V_UV(1));
    o.e = float4(V_UV(2), 0.0, keep_in);
    stream.Append(o);
}

#else

// ---- the strand pixel shader's inputs ------------------------------------------------------------------------------------------
// t1: the engine's final DEVICE depth (the depth buffer of the recorded draw, a shader view of its typeless texture; unbound = a format
// no shader can read: the linear-depth fallback below). t2: the recorded draw's own colour atlas (its pixel shader's t0: rgb + the
// gloss alpha), the texture the per-strand fur discriminator reads at the strand's ROOT uv. t3: the per-draw coat statistics (ps_stats:
// texel (record, 0) and (record, 1)). t4 (ps_stats only): the uv-space features of every record (ps_feat).
Texture2D<float> dev_depth_ps : register(t1);
Texture2D<float4> atlas : register(t2);
Texture2D<float4> coat_stats : register(t3);
Texture2D<float4> feat_tex : register(t4);

// b0 of the strand pixel shader (shellfur::ps_cb_values, ps_rect_values, ps_ext_values).
cbuffer shell_ps : register(b0)
{
    float4 q0; // x strand cells per uv unit (densest level), y root diameter (share of a cell), z smallest strand cell in px, w clump
    float4 q1; // x absolute depth tolerance of the fragment test (normalised), y its relative part (share of the depth),
               // z smallest strand width in px, w directional clumping (lean of a clump's strands, cells at the tip)
    float4 q2; // x excluded rectangles of THIS draw (0..16), y their feather (uv), z this draw's record index (its coat statistics),
               // w 1 = the texel tests of the fur discriminator on (shell_fur_mask not off)
    float4 q_rect[16]; // u0 v0 u1 v1
    float4 q3; // x root device-depth tolerance, y / z the linear-depth fallback of the root test (relative, absolute normalised),
               // w v8: the fragment test's absolute tolerance in metres (FRAG_TOL_M; its relative part is q1.y)
    float4 q4; // v8, this draw's depth bias (its rasterizer state): x DepthBias in depth units, y SlopeScaledDepthBias, z DepthBiasClamp
               // (0 = none), w v18: 1 = the draw's atlas is a proven charr atlas (shellfur::mixed_flag; ft_mixed_weight)

};

// shell_uv_exclude on the fragment's OWN mesh uv (= its root's: the shells share the base triangle's uv): 0 inside a rectangle, 1
// outside, soft over the feather like the fur mask (furmask.rs fd_weight). Every layer of an excluded root goes.
float rect_keep(float2 uv)
{
    int n = (int)clamp(q2.x, 0.0, 16.0);
    float fe = max(q2.y, 1e-5);
    float2 u = frac(uv);
    float w = 1.0;
    [loop] for (int k = 0; k < n; k++)
    {
        float4 r = q_rect[k];
        float d = min(min(u.x - r.x, r.z - u.x), min(u.y - r.y, r.w - u.y));
        w *= 1.0 - smoothstep(-fe, fe, d);
    }
    return w;
}

// ---- the fur discriminator per strand (docs/shell-fur.md "v7") -------------------------------------------------------------------
// Features of the atlas at a uv: the 5x5 texels around the nearest texel at the mip with at most 1024 texels across (Load at a fixed
// mip, wrapping: a pure function of the uv, no flicker): x the mean gloss alpha, y the saturation and z the value (brightest channel)
// of the mean colour, w the colour flatness (furmask.rs fd_flat: rms of rgb x2 / rms of the mean colour, 1 for clipped white); `hue` in
// degrees. false = nothing bound. Mirror: shellfur::texel_features.
bool ft_feat(Texture2D<float4> t, float2 uv, out float4 ft, out float hue)
{
    ft = float4(0.0, 0.0, 0.0, 0.0);
    hue = 0.0;
    uint w, h, n;
    t.GetDimensions(0, w, h, n);
    if (w == 0u || h == 0u || n == 0u)
        return false;
    uint lv = 0u;
    [loop] for (uint k = 0u; k < 15u; k++)
    {
        if (lv + 1u < n && (max(w, h) >> lv) > 1024u)
            lv++;
    }
    int2 m = int2(max(w >> lv, 1u), max(h >> lv, 1u));
    int2 c = int2(floor(frac(uv) * float2(m)));
    float3 s = float3(0.0, 0.0, 0.0);
    float s2 = 0.0, sa = 0.0;
    [unroll] for (int y = -2; y <= 2; y++)
    {
        [unroll] for (int x = -2; x <= 2; x++)
        {
            int2 p = ((c + int2(x, y)) % m + m) % m;
            float4 v = t.Load(int3(p, lv));
            s += v.rgb;
            s2 += dot(v.rgb, v.rgb);
            sa += v.a;
        }
    }
    float3 mean = s / 25.0;
    float mx = max(max(mean.r, mean.g), mean.b);
    float mn = min(min(mean.r, mean.g), mean.b);
    float var = max(s2 / 25.0 - dot(mean, mean), 0.0) / 3.0;
    float fl = mx > 0.97 ? 1.0 : saturate(2.0 * sqrt(var) / max(sqrt(dot(mean, mean) / 3.0), 0.05));
    float d = mx - mn;
    if (d > 1e-5)
    {
        float hh = mx == mean.r ? (mean.g - mean.b) / d : (mx == mean.g ? 2.0 + (mean.b - mean.r) / d : 4.0 + (mean.r - mean.g) / d);
        hue = frac(hh / 6.0 + 1.0) * 360.0;
    }
    ft = float4(saturate(sa / 25.0), d / max(mx, 1e-3), mx, fl);
    return ft.x == ft.x && ft.w == ft.w;
}

// The colour flatness of a window: n texels, s = sum of rgb, s2 = sum of dot(rgb, rgb) (ft_feat's formula).
float ft_flat(float3 s, float s2, float n)
{
    float3 mean = s / n;
    float mx = max(max(mean.r, mean.g), mean.b);
    float var = max(s2 / n - dot(mean, mean), 0.0) / 3.0;
    return mx > 0.97 ? 1.0 : saturate(2.0 * sqrt(var) / max(sqrt(dot(mean, mean) / 3.0), 0.05));
}

// v8: ft_feat at uv, and ALSO the four 5x5 windows FT_DIL texels away diagonally (one 11 x 11 block, 109 loads): ft.x = the HIGHEST
// mean gloss and ft.w = the LOWEST flatness of the five windows (the flat and the glossy test are monotonic, so this is the minimum of
// their weights over the windows): a root within ~3 texels (at the ~1024 level) of a horn or a claw is not fur either. The horn's
// border texels, whose own 5x5 window still reaches into the fur, were the residue at the horn's edge and base; the claws' matte rim
// the strands on the paw claws. The colour tests keep the centre window (dilating them cost coat on spotted / marked coats). Mirror:
// shellfur::texel_features_dilated.
bool ft_feat_dil(Texture2D<float4> t, float2 uv, out float4 ft, out float hue)
{
    ft = float4(0.0, 0.0, 0.0, 0.0);
    hue = 0.0;
    uint w, h, n;
    t.GetDimensions(0, w, h, n);
    if (w == 0u || h == 0u || n == 0u)
        return false;
    uint lv = 0u;
    [loop] for (uint k = 0u; k < 15u; k++)
    {
        if (lv + 1u < n && (max(w, h) >> lv) > 1024u)
            lv++;
    }
    int2 m = int2(max(w >> lv, 1u), max(h >> lv, 1u));
    int2 c = int2(floor(frac(uv) * float2(m)));
    float3 s = float3(0.0, 0.0, 0.0);
    float s2 = 0.0, sa = 0.0;
    float3 cs[4] = { float3(0.0, 0.0, 0.0), float3(0.0, 0.0, 0.0), float3(0.0, 0.0, 0.0), float3(0.0, 0.0, 0.0) };
    float4 cs2 = float4(0.0, 0.0, 0.0, 0.0), csa = float4(0.0, 0.0, 0.0, 0.0);
    [unroll] for (int y = -FT_DIL - 2; y <= FT_DIL + 2; y++)
    {
        [unroll] for (int x = -FT_DIL - 2; x <= FT_DIL + 2; x++)
        {
            bool inc = abs(x) <= 2 && abs(y) <= 2;
            bool in0 = abs(x - FT_DIL) <= 2 && abs(y - FT_DIL) <= 2;
            bool in1 = abs(x + FT_DIL) <= 2 && abs(y - FT_DIL) <= 2;
            bool in2 = abs(x - FT_DIL) <= 2 && abs(y + FT_DIL) <= 2;
            bool in3 = abs(x + FT_DIL) <= 2 && abs(y + FT_DIL) <= 2;
            if (inc || in0 || in1 || in2 || in3)
            {
                int2 p = ((c + int2(x, y)) % m + m) % m;
                float4 v = t.Load(int3(p, lv));
                float vv = dot(v.rgb, v.rgb);
                if (inc) { s += v.rgb; s2 += vv; sa += v.a; }
                if (in0) { cs[0] += v.rgb; cs2.x += vv; csa.x += v.a; }
                if (in1) { cs[1] += v.rgb; cs2.y += vv; csa.y += v.a; }
                if (in2) { cs[2] += v.rgb; cs2.z += vv; csa.z += v.a; }
                if (in3) { cs[3] += v.rgb; cs2.w += vv; csa.w += v.a; }
            }
        }
    }
    float3 mean = s / 25.0;
    float mx = max(max(mean.r, mean.g), mean.b);
    float mn = min(min(mean.r, mean.g), mean.b);
    float fl = ft_flat(s, s2, 25.0);
    fl = min(min(fl, ft_flat(cs[0], cs2.x, 25.0)), min(ft_flat(cs[1], cs2.y, 25.0), min(ft_flat(cs[2], cs2.z, 25.0), ft_flat(cs[3], cs2.w, 25.0))));
    float gl = max(sa, max(max(csa.x, csa.y), max(csa.z, csa.w))) / 25.0;
    float d = mx - mn;
    if (d > 1e-5)
    {
        float hh = mx == mean.r ? (mean.g - mean.b) / d : (mx == mean.g ? 2.0 + (mean.b - mean.r) / d : 4.0 + (mean.r - mean.g) / d);
        hue = frac(hh / 6.0 + 1.0) * 360.0;
    }
    ft = float4(saturate(gl), d / max(mx, 1e-3), mx, fl);
    return ft.x == ft.x && ft.w == ft.w;
}

// v9: a root on a fur triangle NEAR an excluded part (the island analysis: it shares a vertex position with a horn / claw island)
// also reads the four 5x5 windows FT_NEAR texels away diagonally (100 more loads, only for those few triangles' covering fragments):
// ft.x = the highest gloss, ft.w = the lowest flatness, like ft_feat_dil. Mirror: shellfur::texel_features_near.
static const int FT_NEAR = 6;
void ft_near(Texture2D<float4> t, float2 uv, inout float4 ft)
{
    uint w, h, n;
    t.GetDimensions(0, w, h, n);
    if (w == 0u || h == 0u || n == 0u)
        return;
    uint lv = 0u;
    [loop] for (uint k = 0u; k < 15u; k++)
    {
        if (lv + 1u < n && (max(w, h) >> lv) > 1024u)
            lv++;
    }
    int2 m = int2(max(w >> lv, 1u), max(h >> lv, 1u));
    int2 c = int2(floor(frac(uv) * float2(m)));
    [unroll] for (int q = 0; q < 4; q++)
    {
        int2 o = int2((q & 1) != 0 ? FT_NEAR : -FT_NEAR, (q & 2) != 0 ? FT_NEAR : -FT_NEAR);
        float3 s = float3(0.0, 0.0, 0.0);
        float s2 = 0.0, sa = 0.0;
        [unroll] for (int y = -2; y <= 2; y++)
        {
            [unroll] for (int x = -2; x <= 2; x++)
            {
                int2 p = ((c + o + int2(x, y)) % m + m) % m;
                float4 v = t.Load(int3(p, lv));
                s += v.rgb;
                s2 += dot(v.rgb, v.rgb);
                sa += v.a;
            }
        }
        ft.w = min(ft.w, ft_flat(s, s2, 25.0));
        ft.x = max(ft.x, saturate(sa / 25.0));
    }
}

float ft_box(float lo, float hi, float soft, float x)
{
    return smoothstep(lo - soft, lo + soft, x) * (1.0 - smoothstep(hi - soft, hi + soft, x));
}

// Distance of two hues in degrees (0..180).
float ft_hue_dist(float a, float b)
{
    float d = abs(a - b) % 360.0;
    return min(d, 360.0 - d);
}

// The draw is fur at all (the charr's skin draws, not the cloth / metal armour, not a human's skin): the share of its uv footprint whose
// gloss alpha lies in the fur band 0.33..0.50 (every charr skin region measured 0.67..0.97, human skin 0.01..0.30, the armour regions
// mostly below 0.5), and painted, not flat (the median flatness). s0 = (band share, median flatness, cells, -).
float ft_draw_weight(float4 s0)
{
    if (!(s0.z >= 16.0))
        return 0.0; // no footprint (no atlas, no uv shader yet): no shells (fail closed)
    return smoothstep(0.40, 0.60, s0.x) * smoothstep(0.03, 0.05, s0.y);
}

// v18, a MIXED draw: a draw of an atlas that another draw of the same character proved charr fur (q4.w = 1, furselect.rs), whose
// footprint is only partly fur (a body drawn with its armour: bare arms, legs, tail beside the armour pieces) and so fails the draw
// test above. It gets fur where its texels are fur: some of its footprint in the gloss band (shellfur::MIXED_BAND), then per texel
// the band itself and the gloss test against the fur's own gloss (its coat medians are the armour's; shellfur::MIXED_COAT_GLOSS).
// Mirror: shellfur::mixed_weight / strand_weight.
float ft_mixed_weight(float4 s0)
{
    if (!(s0.z >= 16.0))
        return 0.0;
    return smoothstep(0.04, 0.08, s0.x);
}
static const float MIXED_COAT_GLOSS = 0.44;

// One texel at a strand's root: 1 = fur, 0 = horn / claw / fang / lips / gums. s0 / s1 = the draw's coat statistics (medians over its
// uv footprint: s1 = gloss, saturation, value, hue). Every test is RELATIVE to the draw's own coat, so a coat of any colour keeps its
// strands (shellfur::texel_weight is the mirror; the thresholds are measured on 11 real charr atlases, docs/shell-fur.md "v7"):
//  * flat: the horn is a flat dye colour, painted fur never is: flatness below min(0.025, 0.4 x the coat's median) (a uniformly flat coat
//    lowers the threshold with it);
//  * glossy: keratin (claws, fang, nails) and wet parts are glossier than the coat in every atlas: alpha above the coat's + 0.03..0.07;
//  * keratin colour: the claws and the fang have one undyed tan (hue 15..55, saturation 0.22..0.65) on every charr; a texel of that colour
//    that differs from the coat (saturation by 0.1..0.2 or brighter by 1.3..1.6x) is not fur, only where the coat itself is not of that
//    colour (a pale / grey / blue coat, or a coat of another hue; an orange coat relies on the gloss);
//  * red: lips, gums, the mouth, the eye: red hue, saturated, more saturated than the coat or of another hue than the coat.
float ft_texel_weight(float4 ft, float hue, float4 s0, float4 s1, bool colour)
{
    float w = 1.0;
    float th = min(0.025, 0.4 * s0.y);
    w *= smoothstep(0.5 * th, 1.5 * th, ft.w);
    w *= 1.0 - smoothstep(s1.x + 0.03, s1.x + 0.07, ft.x);
    float differs = max(smoothstep(0.10, 0.20, abs(ft.y - s1.y)), smoothstep(0.35, 0.65, log2((ft.z + 0.02) / (s1.z + 0.02))));
    float gate = max(1.0 - smoothstep(0.28, 0.38, s1.y), smoothstep(18.0, 28.0, ft_hue_dist(s1.w, 32.0)));
    float ker = ft_box(15.0, 55.0, 5.0, hue) * ft_box(0.22, 0.65, 0.04, ft.y) * ft_box(0.25, 0.90, 0.04, ft.z) * differs * gate;
    float red_differs = max(smoothstep(0.15, 0.25, ft.y - s1.y), smoothstep(10.0, 18.0, ft_hue_dist(hue, s1.w)) * smoothstep(0.35, 0.45, ft.y));
    float red = (1.0 - smoothstep(15.0, 25.0, ft_hue_dist(hue, 0.0))) * smoothstep(0.30, 0.40, ft.y) * red_differs;
    if (colour) // v15: off on the triangles of a limb island that only its colours excluded (a two-tone coat; furisland.rs LIMB_*)
    {
        w *= 1.0 - ker;
        w *= 1.0 - red;
    }
    return saturate(w);
}

// The discriminator of a strand rooted at uv (the fragment's own mesh uv = its root's): the draw weight x the texel weight. q2.w = 0
// (shell_fur_mask=off) keeps only the draw weight. v9: `near` = the root's triangle touches an excluded island (ft_near): 1; v15: 2 = a
// triangle of a two-tone limb island (no keratin / red colour tests).
float strand_fur(float2 uv, float near_part)
{
    int rec = (int)clamp(q2.z, 0.0, 255.0); // shellfur::MAX_RECS - 1 (v16)
    float4 s0 = coat_stats.Load(int3(rec, 0, 0));
    float4 s1 = coat_stats.Load(int3(rec, 1, 0));
    float w = ft_draw_weight(s0);
    float wm = q4.w > 0.5 ? ft_mixed_weight(s0) : 0.0;
    bool mixed = wm > w;
    if (!mixed && (!(w > 0.0) || q2.w < 0.5))
        return w;
    float4 ft;
    float hue;
    if (!ft_feat_dil(atlas, uv, ft, hue))
        return 0.0;
    if (near_part > 0.5 && near_part < 1.5)
        ft_near(atlas, uv, ft);
    if (mixed)
    {
        float band = ft_box(0.33, 0.50, 0.02, ft.x);
        if (q2.w < 0.5)
            return wm * band;
        float4 s1m = float4(MIXED_COAT_GLOSS, s1.yzw);
        return wm * band * ft_texel_weight(ft, hue, s0, s1m, false);
    }
    return w * ft_texel_weight(ft, hue, s0, s1, !(near_part > 1.5));
}

// ---- the root's visibility (v7: the transparency fix) ------------------------------------------------------------------------
// A strand is only drawn when its ROOT is the surface the screen shows at the root's pixel (its anchor): the composite takes the
// strand's colour from the screen at the anchor, so a root hidden behind a cape, an arm or another character would paint THAT surface's
// colour along the strand (the "transparent" NPCs and the ghosted cloth). Unit-free with the device depth: the engine's depth at the
// anchor (the best of the 3x3 pixels around it) within a few depth steps + the local slope of the root's own depth; without a readable
// device depth the linear depth with the unit's tolerance.
float dev_at(int2 p, int2 mx)
{
    return dev_depth_ps.Load(int3(clamp(p, int2(0, 0), mx), 0));
}

// v8: the expected engine depth at a pixel is the BASE triangle's own device-depth plane there (pl.xy = its gradient per pixel; zdev =
// the plane at the anchor), not "the root's depth + twice the slope": v7's slope term (the shell triangle's derivative, capped at 5e-4)
// allowed several cm at a grazing triangle, so a root right behind an arm, a cape, the floor or a staff "matched" that surface and its
// strand took ITS colour (the tail fur seen "through" the arm, the floor-coloured streaks under the tail, the yellow band of the staff).
// The tolerance is the D24 rounding + this draw's depth bias (q4: constant, slope-scaled, clamped) + 5 % of a pixel of the plane's slope
// (float rounding of the anchor) + pl.z (a triangle too small for a plane).
bool root_visible(float2 apx, float zdev, float4 pl, float zlin, out float za, out float dmatch)
{
    uint w, h;
    dev_depth_ps.GetDimensions(w, h);
    int2 b = int2(floor(apx));
    dmatch = zdev;
    if (w > 0u && h > 0u)
    {
        int2 mx = int2(w, h) - 1;
        float gmax = max(abs(pl.x), abs(pl.y));
        float sb = abs(q4.y) * gmax;
        if (q4.z > 0.0)
            sb = min(sb, q4.z);
        float tol = q3.x + abs(q4.x) + sb + 0.05 * (abs(pl.x) + abs(pl.y)) + pl.z;
        float best = 1e9;
        int2 bp = b;
        [unroll] for (int y = -1; y <= 1; y++)
        {
            [unroll] for (int x = -1; x <= 1; x++)
            {
                int2 p = b + int2(x, y);
                float e = zdev + dot(pl.xy, float2(p) + 0.5 - apx);
                float d = abs(dev_at(p, mx) - e);
                if (d < best)
                {
                    best = d;
                    bp = p;
                }
            }
        }
        // The root's visible linear depth: at the pixel that matched (not at the anchor itself, which at an occluder's edge can show
        // the occluder: the fragment test below would then measure the strand from the occluder's depth).
        bp = clamp(bp, int2(0, 0), mx);
        za = lin_at(float2(bp) + 0.5);
        dmatch = dev_at(bp, mx);
        return best <= tol;
    }
    za = lin_at(apx);
    return valid_depth(za) && abs(za - zlin) <= q3.y * zlin + q3.z;
}

// v8: the strand fragment against the engine's FINAL DEVICE depth at its pixel (what was really drawn there, including what the linear
// depth target never received: geometry drawn after the engine wrote its linear depth, e.g. banners, decorations, alpha-tested cloth):
// true = a surface nearer than the root (or the fragment, for a strand leaning away) by more than FRAG_TOL_M + FRAG_REL_TOL x depth.
// Device depths become view depths with the 1 / w form of a standard projection, anchored at the root itself (w_root (1 - z_root) is
// the near plane's distance): w = w_root (1 - z_root) / (1 - d). pl.w = 0 (another projection) or no device depth: no test (the linear
// test still runs).
bool hidden_by_device_depth(float2 pos, float zdev, float4 pl, float w_root, float w_frag)
{
    uint w, h;
    dev_depth_ps.GetDimensions(w, h);
    if (w == 0u || h == 0u || !(pl.w > 0.0))
        return false;
    float near_d = w_root * (1.0 - zdev);
    if (!(near_d > 0.0))
        return false;
    float dp = dev_at(int2(floor(pos)), int2(w, h) - 1);
    if (!(dp < 1.0))
        return false; // the far plane / sky
    float ws = near_d / max(1.0 - dp, 1e-9);
    float wref = max(w_root, w_frag);
    return ws < wref - (pl.w * q3.w + q1.y * wref);
}

// The coverage (0..1) of the strand of the cell lattice `dl` (cells per uv unit) at uv, at shell height h; px = cells per pixel (the
// footprint). Each cell holds one strand: a cone with a random root position pulled a little towards the centre of its 4x4 clump, the
// clump's strands leaning the same way towards their tips (combed), a random length (60..100 % of the shell height), a root diameter of
// q0.y cells tapering to 40 % at the tip. The edge is box-filtered over one pixel, a strand thinner than q1.z px is drawn q1.z px wide
// and fainter by the same ratio (the same ink: no sub-pixel strand that blinks on and off when it moves), the last quarter of the
// strand fades out (soft tips).
float strand_cov(float2 uv, float dl, float h, float px)
{
    float2 qv = uv * dl;
    int2 cell = (int2)floor(qv);
    float2 f = qv - (float2)cell;
    uint2 cu = (uint2)cell;
    float2 centre = float2(shell_rand(cu, 11u), shell_rand(cu, 23u)) * 0.6 + 0.2;
    int2 clump = cell >> 2;
    uint2 cl = (uint2)clump;
    float2 cc = (float2(shell_rand(cl, 31u), shell_rand(cl, 47u)) + (float2)clump) * 4.0 - (float2)cell;
    centre = lerp(centre, clamp(cc, -0.5, 1.5), saturate(q0.w) * 0.5);
    float2 lean = float2(shell_rand(cl, 71u), shell_rand(cl, 83u)) * 2.0 - 1.0;
    centre += lean * (q1.w * h);
    float len = 0.6 + 0.4 * shell_rand(cu, 59u);
    if (h > len)
        return 0.0;
    float r = 0.5 * saturate(q0.y) * (1.0 - 0.6 * h / len);
    float rmin = 0.5 * max(q1.z, 0.0) * px;
    float re = max(r, rmin);
    float ink = r / max(re, 1e-6);
    float dist = length(f - centre);
    float edge = saturate((re - dist) / max(px, 1e-6) + 0.5);
    float tip = smoothstep(0.0, 1.0, saturate((len - h) / (0.25 * len)));
    return edge * ink * tip;
}

// The strand pixels of one shell. The mesh uv is cut into cells (q0.x per uv unit at the densest level, coarser by powers of two where
// a cell would be smaller than q0.z px on the screen; the two levels around the right size are blended by coverage, so a level change
// is a fade, not a pop).
// v7 (the transparency fix, docs/shell-fur.md): (1) the ROOT must be visible at its anchor (root_visible), else the whole strand goes
// (fail open: no strand rather than a strand of the wrong colour); (2) the fragment is dropped where the frame's final linear depth at
// the pixel is nearer than the root's visible depth (or than the fragment itself, measured from it, for a strand leaning away) by more
// than q1.x + q1.y x depth (5 mm + 0.3 %): a cape, a bracer or another character in front of the root hides every strand over it, also
// when it lies within the strand length (the old test allowed the root's depth + the whole strand reach, 4.5 cm + 0.6 %, and let
// strands paint over armour lying on the fur: the ghosted cloth); the sky and the body behind (a convex coat) do not; (3) the fur discriminator per strand at the
// root's own uv (strand_fur): every layer of a strand rooted on a horn, a claw, the fang, the lips goes, wherever the root projects.
// Output: the packed key asfloat(hq << 24 | a << 20 | dx << 10 | dy) with hq = 1 + round(h * 125) (the shell, 1..126), a = the
// coverage x the LOD fade in 1/15 steps (1..15; below 1/30 the fragment is dropped) and dx / dy the anchor offset from this pixel in
// quarter pixels + 512 (10 bits each: +-128 px, the geometry shader's clamp MAX_PX x 2 at most).
// Which fragment a pixel keeps: the one with the best SCORE, through the mod's own depth buffer (SV_Depth = 1 - score / 2, test LESS):
// score = a (0.75 + 0.25 h) for a strand, 0.2 x fade for the base layer. Equal scores keep the first fragment in draw order.
static const float FRONT_DEPTH_ABS = 4.0; // shellfur::FRONT_DEPTH_ABS (x q1.x = FRAG_TOL_M: 2 cm)
static const float FRONT_DEPTH_REL = 2.0; // shellfur::FRONT_DEPTH_REL (x q1.y = FRAG_REL_TOL: 0.6 %)
float ps_shell(GOut i, out float depth : SV_Depth) : SV_Target
{
    float h = saturate(i.b.x);
    float fade = saturate(i.b.z);
    float2 uv = i.a.zw;
    float2 dx = ddx(uv), dy = ddy(uv); // before any discard
    float zr = i.b.y;
    float za, dmatch;
    if (!root_visible(i.a.xy, i.b.w, i.d, zr, za, dmatch))
        discard;
    if (!valid_depth(za))
        za = zr;
    float zf = za - (zr - i.c.x); // the fragment's depth: the root's visible depth minus the extrusion towards the camera
    float zref = max(za, zf);      // nothing nearer than the root (or than the fragment, for a strand leaning away) may lie under it
    float zp = lin_at(i.pos.xy);
    if (valid_depth(zp) && zp < zref - (q1.x + q1.y * zref))
        discard;
    if (hidden_by_device_depth(i.pos.xy, i.b.w, i.d, i.c.y, i.c.z))
        discard;
    // Strand cell size on the screen: uv change per pixel.
    float gpx = max(max(length(float2(dx.x, dy.x)), length(float2(dx.y, dy.y))), 1e-9);
    float dens = max(q0.x, 1.0);
    float cell_px = 1.0 / (dens * gpx);
    float lvl = clamp(log2(max(q0.z, 0.5) / cell_px), 0.0, 24.0);
    float l0 = floor(lvl);
    float t = lvl - l0;
    float d0 = dens / exp2(l0);
    float cov = 1.0; // the base layer (h = 0) covers the whole surface: the composite darkens the skin between the strands
    if (h > 0.0)
        cov = lerp(strand_cov(uv, d0, h, d0 * gpx), strand_cov(uv, d0 * 0.5, h, d0 * 0.5 * gpx), t);
    cov *= fade * rect_keep(uv);
    if (floor(cov * 15.0 + 0.5) < 1.0)
        discard;
    cov *= strand_fur(uv, i.c.w); // only for the fragments that cover (109 texel loads; v9 near: 209)
    float aq = floor(cov * 15.0 + 0.5);
    if (aq < 1.0)
        discard;
    float score = h > 0.0 ? cov * (0.75 + 0.25 * h) : 0.2 * cov;
    depth = 1.0 - 0.5 * saturate(score) - 1e-4;
    float hq = clamp(1.0 + floor(h * 125.0 + 0.5), 1.0, 126.0);
    float2 off = clamp(floor((i.a.xy - i.pos.xy) * 4.0 + 0.5) + 512.0, 0.0, 1023.0);
    uint key = ((uint)hq << 24) | ((uint)aq << 20) | ((uint)off.x << 10) | (uint)off.y;
    // v13 (KEY_FRONT, shellfur::KEY_FRONT): the surface under this strand fragment lies clearly BEHIND its root (FRONT_DEPTH_ABS x
    // the fragment tolerance + FRONT_DEPTH_REL x the relative one: 2 cm + 0.6 %): the strand floats in front of it (head / neck fur
    // over the horn behind, a silhouette over another part): the composite must not hold it to that surface's colour (the detail test).
    if (h > 0.0 && valid_depth(zp) && zp > zref + FRONT_DEPTH_ABS * q1.x + FRONT_DEPTH_REL * q1.y * zref)
        key |= 0x80000000u;
    return asfloat(key);
}

// ---- v11: the SKIN pass (docs/shell-fur.md "v11") ---------------------------------------------------------------------------
// The old fur look (fur_look) and the rim light (rim_look) of light_adjust_hdr.hlsl used to run on EVERY pixel of the fur / rim mask
// (every draw with a fur pixel shader: hair cards, horns, claws, armour, other races), and since the shells REPLACE the old look only
// where a strand key lies, the pixels the shell classification rejected (the hair cards: "not fur"; the horn: an excluded island) were
// exactly the ones left to the old strand modulation: the checkered hair and the striped horn seen in the game. The skin pass draws
// every recorded draw once more, base layer only (gs_shell with g5.x = 1, the island bits applied: an excluded triangle emits
// nothing), into an R8 target with a MAX blend: per pixel how much the VISIBLE surface is proven fur skin = the draw weight x the texel
// weight (strand_fur at the pixel's own uv, the same verdict the strands get) x shell_uv_exclude. light_adjust_hdr.hlsl multiplies the
// old look's and the rim's gate by it (t25). Zero (nothing written) where: the draw is not fur (hair cards 0.54 gloss, armour, human
// skin), the triangle belongs to an excluded island (horn, claws, fang), the texel is not fur, the root is not the visible surface
// (an alpha-tested card's cut-out: the engine's depth there is the surface behind it), or the fur mask at the pixel holds ANOTHER
// draw's uv (a hair card masked "solid" over the head: its uv would give the head's fur a strand pattern at the card's scale).
// t4 = the fur mask (R16G16B16A16_UNORM, rg = frac(uv), b = 1 with a uv; furmask.rs), only in this pass (ps_stats uses t4 for the
// features). Unbound: no uv check.
static const float SKIN_UV_TOL = 0.004; // shellfur::SKIN_UV_TOL: the same uv rasterised twice differs by float rounding only
// The base triangle is the visible surface AT THIS PIXEL (root_visible's test without its 3x3 search: a strand may root next to its
// anchor's pixel, the skin must not bleed one pixel into a cut-out hole or past a silhouette).
bool surface_here(float2 pos, float2 apx, float zdev, float4 pl, float zlin)
{
    uint w, h;
    dev_depth_ps.GetDimensions(w, h);
    if (w > 0u && h > 0u)
    {
        int2 mx = int2(w, h) - 1;
        float gmax = max(abs(pl.x), abs(pl.y));
        float sb = abs(q4.y) * gmax;
        if (q4.z > 0.0)
            sb = min(sb, q4.z);
        float tol = q3.x + abs(q4.x) + sb + 0.05 * (abs(pl.x) + abs(pl.y)) + pl.z;
        int2 p = clamp(int2(floor(pos)), int2(0, 0), mx);
        float e = zdev + dot(pl.xy, float2(p) + 0.5 - apx);
        return abs(dev_at(p, mx) - e) <= tol;
    }
    float za = lin_at(pos);
    return valid_depth(za) && abs(za - zlin) <= q3.y * zlin + q3.z;
}

float ps_skin(GOut i) : SV_Target
{
    float2 uv = i.a.zw;
    if (!surface_here(i.pos.xy, i.a.xy, i.b.w, i.d, i.b.y))
        discard;
    uint mw, mh;
    feat_tex.GetDimensions(mw, mh);
    if (mw > 0u && mh > 0u)
    {
        float4 m = feat_tex.Load(int3(clamp(int2(floor(i.pos.xy)), int2(0, 0), int2(mw, mh) - 1), 0));
        if (m.b > 0.5)
        {
            float2 d = abs(frac(uv) - m.rg);
            d = min(d, 1.0 - d);
            if (!(max(d.x, d.y) <= SKIN_UV_TOL))
                discard;
        }
    }
    float w = rect_keep(uv) * strand_fur(uv, i.c.w);
    if (!(w > 0.0))
        discard;
    return saturate(w);
}

// The unit measurement's pixel shader: the triangle's values into the 1x1 target (ADD blend).
float4 ps_measure(MOut i) : SV_Target
{
    return i.v;
}

// ---- the per-draw coat statistics (v7) ----------------------------------------------------------------------------------------
// ps_feat: one cell of a record's 32 x 32 uv-space footprint (gs_uv): the features of the draw's atlas (t2) at the cell's uv, packed
// as (gloss, saturation, value, 2 x floor(hue) + min(flatness, 0.999)). The target is cleared to -1 = not covered.
float4 ps_feat(UOut i) : SV_Target
{
    float4 ft;
    float hue;
    if (!ft_feat(atlas, i.uv.xy, ft, hue))
        return float4(-1.0, -1.0, -1.0, -1.0);
    return float4(ft.xyz, 2.0 * floor(hue) + min(ft.w, 0.999));
}

// A full-screen triangle (the statistics pass).
float4 vs_stats(uint id : SV_VertexID) : SV_Position
{
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.5, 1.0);
}

static const int FT_N = 32; // shellfur::FEAT_N: cells across a record's footprint

// ps_stats: pixel (record, row) of the statistics target: the record's footprint cells reduced to row 0 = (share of cells in the fur
// gloss band 0.33..0.50, median flatness, covered cells, the draw weight) and row 1 = (median gloss, median saturation, median value,
// the coat hue: the saturation-weighted circular mean). Medians from 32-bin histograms, interpolated inside the bin. Mirror:
// shellfur::coat_stats.
float4 ps_stats(float4 pos : SV_Position) : SV_Target
{
    int rec = (int)pos.x;
    int row = (int)pos.y;
    float hist[128]; // 4 x 32 bins: gloss, saturation, value, flatness
    [loop] for (int k = 0; k < 128; k++)
        hist[k] = 0.0;
    float n = 0.0, band = 0.0;
    float2 hsum = float2(0.0, 0.0);
    [loop] for (int y = 0; y < FT_N; y++)
    {
        [loop] for (int x = 0; x < FT_N; x++)
        {
            float4 f = feat_tex.Load(int3(x, rec * FT_N + y, 0));
            if (!(f.x >= 0.0))
                continue;
            float hue = floor(f.w * 0.5);
            float fl = f.w - 2.0 * hue;
            n += 1.0;
            band += (f.x >= 0.33 && f.x <= 0.50) ? 1.0 : 0.0;
            hist[min((int)(saturate(f.x) * 32.0), 31)] += 1.0;
            hist[32 + min((int)(saturate(f.y) * 32.0), 31)] += 1.0;
            hist[64 + min((int)(saturate(f.z) * 32.0), 31)] += 1.0;
            hist[96 + min((int)(saturate(fl) * 32.0), 31)] += 1.0;
            float r = radians(hue);
            hsum += f.y * float2(cos(r), sin(r));
        }
    }
    if (n < 1.0)
        return float4(0.0, 0.0, 0.0, 0.0);
    // The four medians.
    float med[4] = { 1.0, 1.0, 1.0, 1.0 };
    [loop] for (int c = 0; c < 4; c++)
    {
        float acc = 0.0;
        bool done = false;
        [loop] for (int b = 0; b < 32; b++)
        {
            float cnt = hist[c * 32 + b];
            if (!done && acc + cnt >= 0.5 * n && cnt > 0.0)
            {
                med[c] = (b + saturate((0.5 * n - acc) / cnt)) / 32.0;
                done = true;
            }
            acc += cnt;
        }
    }
    if (row == 0)
    {
        float4 s0 = float4(band / n, med[3], n, 0.0);
        s0.w = ft_draw_weight(s0);
        return s0;
    }
    float hue_ref = length(hsum) > 1e-4 ? frac(atan2(hsum.y, hsum.x) / 6.2831853 + 1.0) * 360.0 : 0.0;
    return float4(med[0], med[1], med[2], hue_ref);
}

#endif
