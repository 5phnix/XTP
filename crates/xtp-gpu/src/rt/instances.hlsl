// The table is sorted by the full unsigned 64-bit title address, low word first in memory.
cbuffer Counts : register(b0) { uint instances; }
ByteAddressBuffer table : register(t0);
RWByteAddressBuffer source : register(u0);
RWByteAddressBuffer destination : register(u1);

[RootSignature("RootConstants(num32BitConstants=1,b0), SRV(t0), UAV(u0), UAV(u1)")]
[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint instance = thread.x + thread.y * (65535 * 64);
    if (instance >= instances) return;
    uint at = instance * 64;
    uint2 address = source.Load2(at + 56);
    uint lo = 0, hi = table.Load(0);
    uint2 mapped = 0;
    while (lo < hi) {
        uint mid = lo + (hi - lo) / 2;
        uint4 row = table.Load4((mid + 1) * 16);
        if (all(row.xy == address)) {
            mapped = row.zw;
            break;
        }
        if (row.y < address.y || (row.y == address.y && row.x < address.x)) lo = mid + 1;
        else hi = mid;
    }
    destination.Store4(at, source.Load4(at));
    destination.Store4(at + 16, source.Load4(at + 16));
    destination.Store4(at + 32, source.Load4(at + 32));
    destination.Store2(at + 48, source.Load2(at + 48));
    // A null BLAS makes an unknown instance inactive and preserves the other instance indices.
    destination.Store2(at + 56, mapped);
}
