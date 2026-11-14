// Fur mask writer (src/furmask.rs). The mod re-issues a selected opaque draw of the engine (same vertex shader, same
// input layout, buffers, constant buffers, viewport, rasteriser and depth buffer) into its own R16G16B16A16_UNORM target
// with a pixel shader of its own, so the target ends up with a = 1 exactly where that draw is visible. THIS shader is the
// fallback for a vertex shader whose signature is not known yet: it reads nothing (no input signature, no resources; a pixel
// shader may declare fewer inputs than the vertex shader outputs, but an input it does declare must sit at the vertex
// shader's register, so it cannot even read SV_Position) and writes uv invalid (b = 0), so the fur look has no strands there.
// The shaders that carry the uv (rg = frac(uv), b = 1) and the fragment depth are generated per vertex-shader output
// signature by furmask.rs (uv_ps_hlsl, z_ps_hlsl).
//
// SV_Target1 is the z-stamp (R32_FLOAT, bound beside the mask): the device depth of the NEAREST fragment of the selected
// draws at that pixel (furmask.rs blends it with MIN, MAX for reversed Z: our mask draw writes no depth, so without that the
// stamp would be the LAST fragment the draw rasterised, a farther layer of the same mesh, and the check erased the inside of
// every multi-layer mesh). The occlusion check (ps_occ_* below) compares it with the engine's final depth. A stamp outside
// 0..1 = "nothing stamped" (cleared to 2 for LESS, -1 for GREATER): the check keeps such a pixel as it is (fail open). The
// fallbacks here cannot read the depth, so they stamp -1 (written without the MIN / MAX blend).
struct MaskOut
{
    float4 m : SV_Target0;
    float z : SV_Target1;
};

MaskOut ps_mask()
{
    MaskOut o;
    o.m = float4(0.0, 0.0, 0.0, 1.0);
    o.z = -1.0;
    return o;
}

// The rim light mask (furmask.rs, second channel): an R8_UNORM target, 1 where a rim_ps draw is visible. Same rules as ps_mask.
MaskOut ps_rim()
{
    MaskOut o;
    o.m = float4(1.0, 1.0, 1.0, 1.0);
    o.z = -1.0;
    return o;
}

// ---- the occlusion check (furmask.rs, Mask::validate) ---------------------------------------------------------------------
// A full-screen draw into the mask target (blend off, no depth buffer) at the start of the light-adjust draw, once per frame
// and channel. A mask pixel whose stamped fragment is not the one that ended up visible (a surface drawn LATER in the pass
// covers it: a hood over the head) is set to 0 (all channels); every other pixel is discarded, i.e. left bit-identical.

Texture2D<float> zstamp : register(t0);
// The engine's final depth buffer (R24_UNORM_X8 / R32_FLOAT / R16_UNORM / R32_FLOAT_X8X24 view of its typeless texture).
Texture2D<float> scene_depth : register(t1);

// Smallest absolute tolerance in device depth (furmask.rs OCC_TOL, the same value): the stamp is the fragment depth the pixel shader
// sees, the engine stores the depth rounded to D24 (1 step = 6e-8) and, on a test GPU, with a depth bias the pixel shader does not
// see (a constant 65 steps in the game). 1e-5 (168 steps) is still far below any real gap between a head and a hood (1e-3 and more).
// occ_tol.x (b0, furmask.rs Mask::validate) adds the depth bias of the masked draws' rasterizer state; unbound it reads 0.
static const float OCC_TOL = 1e-5;
cbuffer occ_params : register(b0)
{
    float4 occ_tol;
};

float occ_tolerance()
{
    return max(OCC_TOL, min(occ_tol.x, 2e-4)); // NaN -> OCC_TOL
}

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

// Depth test LESS / LESS_EQUAL: nearer = smaller. NaN anywhere = not occluded (discard = keep).
float4 ps_occ_less(VSOut i) : SV_Target
{
    int3 p = int3(i.pos.xy, 0);
    float s = zstamp.Load(p);
    float z = scene_depth.Load(p);
    if (!(s >= 0.0 && s <= 1.0) || !(z < s - occ_tolerance()))
        discard;
    return float4(0.0, 0.0, 0.0, 0.0);
}

// Depth test GREATER / GREATER_EQUAL (reversed Z): nearer = larger.
float4 ps_occ_greater(VSOut i) : SV_Target
{
    int3 p = int3(i.pos.xy, 0);
    float s = zstamp.Load(p);
    float z = scene_depth.Load(p);
    if (!(s >= 0.0 && s <= 1.0) || !(z > s + occ_tolerance()))
        discard;
    return float4(0.0, 0.0, 0.0, 0.0);
}
