// The indices a host BLAS build reads, checked on the GPU where the build
// reads them (Codex review 11f27db1, P1-2): each geometry's indices are
// copied, as 32-bit ones, into the build's own buffer after its flag word;
// one past the geometry's vertices raises the flag, and then every index
// of the build becomes 0: degenerate triangles, inactive in DXR, so the
// build takes none of them.
cbuffer Job : register(b0) {
    uint count;    // indices of this geometry
    uint wide;     // 32-bit indices, else 16-bit
    uint vertices; // its vertex count
    uint first;    // byte offset of its indices in the build's buffer
    uint offset;   // byte offset of its first index in the source window
    uint pass;     // 0: clear the flag, 1: copy and check, 2: zero if flagged
};
ByteAddressBuffer source : register(t0);
RWByteAddressBuffer built : register(u0);

[RootSignature("RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0)")]
[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint i = thread.x + thread.y * (65535 * 64);
    if (pass == 0) {
        if (i == 0) built.Store(0, 0);
        return;
    }
    if (i >= count) return;
    uint at = first + 4 * i;
    if (pass == 1) {
        uint index;
        if (wide != 0) {
            index = source.Load(offset + 4 * i);
        } else {
            uint b = offset + 2 * i;
            uint word = source.Load(b & ~3u);
            index = (b & 2) != 0 ? word >> 16 : word & 0xffff;
        }
        if (index >= vertices) {
            uint was;
            built.InterlockedOr(0, 1, was);
        }
        built.Store(at, index);
    } else if (built.Load(0) != 0) {
        built.Store(at, 0);
    }
}
