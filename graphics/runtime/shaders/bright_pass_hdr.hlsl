// HDR replacements for two engine pixel shaders, one entry point each:
//   bright_pass_downsample -> ps#2ed4a75f (#10/#11 bright-pass downsample)
//   bloom_threshold        -> ps#14b1e90e (#14 bloom threshold)
// Same math, input signature (TEXCOORD0..3 = four tap positions), t0/s0 slot and cb0 layout as the
// originals; the upper clamp on colour is gone (add_sat / mad_sat became max(x, 0), which also turns
// NaN into 0). Alpha stays saturated like the original (mov_sat o0.w).
//
// cb0[0].xyz = subtracted threshold.
//
// BLOOM_STRENGTH (prepended as a #define by hdr_shaders::bloom_hlsl; 1.0 = the game's bloom) scales
// the colour that bloom_threshold writes, i.e. how much bloom is added later. Without the define the
// output is unchanged. Bloom energy above the game's own range (1.0) is whitened, see bloom_threshold.

#ifndef BLOOM_STRENGTH
#define BLOOM_STRENGTH 1.0
#endif

cbuffer cb0 : register(b0)
{
    float4 threshold; // .xyz
};

// Bound by the mod at the draw of bloom_threshold (b6, see src/bloomfix.rs): xyz = per-channel factors that divide the
// combine pass's bloom tint out (the combine multiplies the bloom by that tint), w > 0.5 = valid. UNBOUND it reads
// zero = no compensation, which is what the equivalence tests pin.
cbuffer bloom_comp : register(b6)
{
    float4 comp;
};

Texture2D t0 : register(t0);
SamplerState s0 : register(s0);

struct BPIn
{
    float2 p0 : TEXCOORD0; // v0.xy
    float2 p1 : TEXCOORD1; // v0.zw
    float2 p2 : TEXCOORD2; // v1.xy
    float2 p3 : TEXCOORD3; // v1.zw
};

// VANILLA (prepended as a #define while the user has the mod switched off): the scene targets stay float after the
// switch-off, so effects stacked above 1.0 would feed MORE energy into the bloom than the 8-bit engine ever saw.
// Clipping every tap at 1.0 gives back exactly what the UNORM target delivered.
float4 tap(float2 p)
{
    float4 v = max(t0.Sample(s0, p), 0.0); // negative / NaN texels of the float scene count as 0 (the 8-bit target clamped)
#ifdef VANILLA
    v = saturate(v);
#endif
    return v;
}

float4 average4(BPIn i)
{
    float4 r = tap(i.p1) * 0.25;
    r = tap(i.p0) * 0.25 + r;
    r = tap(i.p2) * 0.25 + r;
    r = tap(i.p3) * 0.25 + r;
    return r;
}

float4 bright_pass_downsample(BPIn i) : SV_Target
{
    float4 avg = average4(i);
    return float4(max(avg.xyz - threshold.xyz, 0.0), saturate(avg.w));
}

float4 bloom_threshold(BPIn i) : SV_Target
{
    float4 avg = average4(i);
    float Y = dot(avg.xyz, float3(0.2125, 0.7154, 0.0721));
    float3 b = max(avg.xyz * Y - threshold.xyz, 0.0);
    // Strong bloom glows white instead of taking the colour of its brightest source (the combine multiplies it by a
    // tint and adds it to the picture: a bright coloured light would otherwise paint its surroundings in its own
    // colour). The amount of whitening depends on the energy BEFORE the strength, so the output stays linear in
    // BLOOM_STRENGTH; inputs within the game's own range (energy <= 1) are untouched.
    float L = dot(b, float3(0.2125, 0.7154, 0.0721));
    b = lerp(b, float3(L, L, L), saturate((L - 1.0) * 0.5));
    // The combine adds the bloom to the picture in the ENCODED domain, unbounded now that the scene is float: a
    // bright coloured light blooms with enormous energy (hundreds of SDR whites once squared back to linear) and
    // its cores turn into flat saturated paint (measured: blue blobs as soon as the bloom strength passes 0.3).
    // The bloom therefore never exceeds the game's own maximum (largest channel 1.0, hue kept): the strength still
    // scales the tails (dim bloom) freely, while bright cores are the job of the halo, which knows about energy.
    // Within the game's range (largest channel <= 1 after the strength) nothing changes.
    if (comp.w > 0.5)
        b *= comp.xyz;
    b *= BLOOM_STRENGTH;
#ifndef VANILLA
    // Strong bloom (after the strength) turns white whatever the tint reading says: the more energy, the whiter, so a
    // lamp or a skill glow never paints its surroundings cyan. Dim tails keep their colour.
    float Ls = dot(b, float3(0.2125, 0.7154, 0.0721));
    b = lerp(b, float3(Ls, Ls, Ls), 0.85 * saturate((max(max(b.r, b.g), b.b) - 0.35) / 1.0));
#endif
    float mb = max(max(b.r, b.g), b.b);
#ifdef VANILLA
    b *= (mb > 1.0) ? 1.0 / mb : 1.0;
#else
    // Soft knee instead of a hard cap at 1.0: the combine adds this bloom times 2 to the picture, unclipped now, so a
    // big bright surface (white fur, armour, the sun) received up to +2.0 and blew out completely. The largest
    // channel is compressed as mb / (1 + mb / 0.55): slope 1 for dim bloom, 0.35 at 1.0, never above 0.55.
    b *= 1.0 / (1.0 + mb / 0.55);
#endif
    return float4(b, saturate(avg.w));
}
