// A top-level build's instances given as an array of pointers, gathered on the GPU (locum::d3d12::gather). Shader
// model 5.0, for Direct3D 12.
//
// `records` reads the pointers locum copied right after the build (u0) and writes, for pointer `i`, a record of
// locum's command signature (u1): the pointer as a root SRV, `i` as the root constant, and a dispatch of one group, or
// of none for a null pointer. It also writes the pointers as read (u2) and clears each stamp (u3). ExecuteIndirect
// then runs `copy` once per record: the instance the pointer points at, 64 bytes, into place `i` of the instances
// (u0), and `i + 1` into stamp `i` (u3), which says the record ran.

// The root signature of both: the instance a pointer points at, the constant (`count` for `records`, `i` for
// `copy`), and four buffers. A command signature sets root parameters in increasing order: the SRV, then the constant.
#define ROOT "SRV(t0), RootConstants(num32BitConstants = 1, b0), UAV(u0), UAV(u1), UAV(u2), UAV(u3)"

cbuffer Gather : register(b0)
{
    uint k;
};

ByteAddressBuffer instance : register(t0);
RWByteAddressBuffer first : register(u0);
RWByteAddressBuffer records_out : register(u1);
RWByteAddressBuffer pointers_out : register(u2);
RWByteAddressBuffer stamps : register(u3);

// The bytes of one record: the root SRV's address, the constant, and the dispatch's three group counts.
#define RECORD 24

[numthreads(64, 1, 1)]
void records(uint3 id : SV_DispatchThreadID)
{
    uint i = id.x;
    if (i >= k)
    {
        return;
    }
    uint2 p = first.Load2(i * 8);
    pointers_out.Store2(i * 8, p);
    stamps.Store(i * 4, 0);
    records_out.Store2(i * RECORD, p);
    records_out.Store(i * RECORD + 8, i);
    records_out.Store3(i * RECORD + 12, uint3((p.x | p.y) != 0 ? 1 : 0, 1, 1));
}

[numthreads(16, 1, 1)]
void copy(uint3 t : SV_GroupThreadID)
{
    first.Store(k * 64 + t.x * 4, instance.Load(t.x * 4));
    if (t.x == 0)
    {
        stamps.Store(k * 4, k + 1);
    }
}
