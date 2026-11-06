// A multisampled texture's samples, bit for bit (locum_capture::samples). Shader model 5.0, for Direct3D 11 and 12.
//
// Every pass draws one triangle over the image with its `k` in the constant `pass_k` (b0), which the vertex shader
// passes on: a draw's first vertex does not reach SV_VertexID on every driver (NVIDIA's Direct3D 11 gave every
// `Draw(3, 3 * k)` k = 0), a constant does. Reading,
// `k` is `slice * 32 + sample` of the multisampled texture, drawn into that image (one per sample of each slice).
// Writing, `k` is the image to read (`slice * samples + sample`) plus the sample times 2^24 (written alone, through
// SV_Coverage) and, for a stencil pass, the bit plus one times 2^16.
//
// `MS` 0 reads a one-sample texture (sample 0) instead: what tests check the conversions with, against copies.

// Direct3D 12's root signature for every pass: the pass's `k`, and the two views it reads, a table of the pixel
// shader's.
#define ROOT "RootConstants(num32BitConstants = 1, b0, visibility = SHADER_VISIBILITY_VERTEX), "     "DescriptorTable(SRV(t0, numDescriptors = 2), visibility = SHADER_VISIBILITY_PIXEL)"

cbuffer Passes : register(b0)
{
    uint pass_k;
};

struct V
{
    float4 position : SV_Position;
    nointerpolation uint k : K;
};

V vs(uint id : SV_VertexID)
{
    V v;
    uint corner = id % 3;
    float2 at = float2(corner == 2 ? 3.0 : -1.0, corner == 1 ? 3.0 : -1.0);
    v.position = float4(at, 0.0, 1.0);
    v.k = pass_k;
    return v;
}

#ifndef MS
#define MS 1
#endif
#if MS
#define SOURCE(T) Texture2DMSArray<T>
#define LOAD(t, v) t.Load(int3(v.position.xy, v.k >> 5), v.k & 31)
#else
#define SOURCE(T) Texture2DArray<T>
#define LOAD(t, v) t.Load(int4(v.position.xy, v.k >> 5, 0))
#endif

// A unorm24 depth as the integer it is: round(z * (2^24 - 1)), exactly. `z` is that integer over 2^24 - 1, as near as a
// float is; the product is taken in 64 bits (hi, lo), so no float rounding moves it.
uint unorm24(float z)
{
    if (!(z > 0.0))
        return 0;
    if (z >= 1.0)
        return 0xFFFFFF;
    uint bits = asuint(z);
    uint m = (bits & 0x7FFFFF) | 0x800000;
    // z = m * 2^-shift, shift >= 24.
    uint shift = 150 - (bits >> 23);
    if (shift >= 49)
        return 0;
    // m * (2^24 - 1) = (m << 24) - m.
    uint hi = m >> 8;
    uint lo = m << 24;
    hi -= lo < m ? 1 : 0;
    lo -= m;
    // + 2^(shift - 1), to round.
    uint half_bit = shift - 1;
    if (half_bit < 32)
    {
        uint before = lo;
        lo += 1u << half_bit;
        hi += lo < before ? 1 : 0;
    }
    else
    {
        hi += 1u << (half_bit - 32);
    }
    return shift >= 32 ? hi >> (shift - 32) : (lo >> shift) | (hi << (32 - shift));
}

// The float nearest k / (2^24 - 1): for k of p bits, (k * 2^(24 - p) + 1) * 2^(p - 48), whose mantissa is the first
// factor. See unorm24.
float depth24(uint k)
{
    if (k == 0)
        return 0.0;
    uint p = firstbithigh(k) + 1;
    uint m = (k << (24 - p)) + 1;
    if (m == 0x1000000)
        return asfloat((p + 103) << 23);
    return asfloat(((p + 102) << 23) | (m & 0x7FFFFF));
}

SOURCE(uint4) bits_in : register(t0);
uint4 read_bits(V v) : SV_Target
{
    return LOAD(bits_in, v);
}

SOURCE(float4) floats_in : register(t0);
float4 read_floats(V v) : SV_Target
{
    return LOAD(floats_in, v);
}

SOURCE(float) depth_in : register(t0);
SOURCE(uint2) stencil_in : register(t1);
uint read_d24s8(V v) : SV_Target
{
    return unorm24(LOAD(depth_in, v)) | (LOAD(stencil_in, v).g << 24);
}

uint2 read_d32s8(V v) : SV_Target
{
    return uint2(asuint(LOAD(depth_in, v)), LOAD(stencil_in, v).g);
}

Texture2DArray<uint4> images : register(t0);
Texture2DArray<float4> float_images : register(t0);

uint4 image(V v)
{
    return images.Load(int4(v.position.xy, v.k & 0xFFFF, 0));
}

// The one sample a write pass writes.
uint coverage(V v)
{
    return 1u << (v.k >> 24);
}

// The stencil bit a stencil pass writes.
uint bit(V v)
{
    return ((v.k >> 16) & 0xFF) - 1;
}

void write_bits(V v, out uint4 colour : SV_Target, out uint mask : SV_Coverage)
{
    colour = image(v);
    mask = coverage(v);
}

void write_floats(V v, out float4 colour : SV_Target, out uint mask : SV_Coverage)
{
    colour = float_images.Load(int4(v.position.xy, v.k & 0xFFFF, 0));
    mask = coverage(v);
}

void write_d24(V v, out float depth : SV_Depth, out uint mask : SV_Coverage)
{
    depth = depth24(image(v).x & 0xFFFFFF);
    mask = coverage(v);
}

void write_d32(V v, out float depth : SV_Depth, out uint mask : SV_Coverage)
{
    depth = asfloat(image(v).x);
    mask = coverage(v);
}

// Each stencil bit is a pass of its own, writing only that bit (the stencil reference has it): the sample is left
// where the bit is clear.
void write_stencil24(V v, out uint mask : SV_Coverage)
{
    mask = ((image(v).x >> 24) >> bit(v) & 1) ? coverage(v) : 0;
}

void write_stencil32(V v, out uint mask : SV_Coverage)
{
    mask = (image(v).y >> bit(v) & 1) ? coverage(v) : 0;
}
