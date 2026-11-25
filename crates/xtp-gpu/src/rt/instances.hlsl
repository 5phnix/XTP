// The table is sorted by the full unsigned 64-bit title address, low word first in memory.
// stand_in: the BLAS an instance of a BLAS the host does not have names, with mask 0.
cbuffer Counts : register(b0) { uint instances; uint2 stand_in; }
ByteAddressBuffer table : register(t0);
RWByteAddressBuffer source : register(u0);
RWByteAddressBuffer destination : register(u1);

[RootSignature("RootConstants(num32BitConstants=3,b0), SRV(t0), UAV(u0), UAV(u1)")]
[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint instance = thread.x + thread.y * (65535 * 64);
    if (instance >= instances) return;
    uint at = instance * 64;
    uint2 address = source.Load2(at + 56);
    uint lo = 0, hi = table.Load(0);
    uint2 mapped = 0;
    bool found = false;
    while (lo < hi) {
        uint mid = lo + (hi - lo) / 2;
        uint4 row = table.Load4((mid + 1) * 16);
        if (all(row.xy == address)) {
            mapped = row.zw;
            found = true;
            break;
        }
        if (row.y < address.y || (row.y == address.y && row.x < address.x)) lo = mid + 1;
        else hi = mid;
    }
    // Instance ID and mask, then hit group offset and flags.
    uint2 words = source.Load2(at + 48);
    if (!found) {
        // An unknown instance stays inactive, the other instance indices kept:
        // the stand-in BLAS (not a null one, on which WARP faults), mask 0.
        mapped = stand_in;
        words.x &= 0x00ffffffu;
    }
    destination.Store4(at, source.Load4(at));
    destination.Store4(at + 16, source.Load4(at + 16));
    destination.Store4(at + 32, source.Load4(at + 32));
    destination.Store2(at + 48, words);
    destination.Store2(at + 56, mapped);
}
