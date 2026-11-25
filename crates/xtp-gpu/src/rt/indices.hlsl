// The inputs a host BLAS build reads, made on the GPU where the build reads
// them (Codex review 11f27db1, P1-2), into the build's own buffer after its
// flag word (rt/indices.rs):
// - each indexed geometry's indices, as 32-bit ones: copied from 16-bit or
//   32-bit indices, or decoded from the console builder's meshlet-packed
//   ones. One past the geometry's vertices, or a packed one whose parts lie
//   outside its window, raises the flag, and then every index of the build
//   becomes 0: degenerate triangles, which give no intersection, so the
//   build takes none of them.
// - each geometry's vertices: decoded from the console builder's quantized
//   ones to float3 positions, or copied as they are;
// - each geometry's transform, copied.
// The console builder's formats are described in rt/console.rs.
//
// The title's copies are read as UAVs (a shader of the title may have
// written them in the submission, which left them in that state), from a
// view 16-byte aligned, `offset` bytes before the window.
cbuffer Job : register(b0) {
    uint count;    // indices, vertices or words this job makes
    uint kind;     // 0: 16-bit indices, 1: 32-bit, 2: meshlet-packed, 3: quantized vertices, 4: words
    uint vertices; // indices: the geometry's vertex count, which they must be below
    uint first;    // byte offset of its output in the build's buffer
    uint offset;   // byte offset of its window in the source view
    uint step;     // 0: clear the flag, 1: make and check, 2: zero indices if flagged
    uint limit;    // packed: the bytes of its window it may read
    uint flip;     // packed: each triangle's first and third vertices swapped
    uint stride;   // quantized: bytes from one vertex to the next
};
RWByteAddressBuffer source : register(u0);
RWByteAddressBuffer built : register(u1);

// Flag bits: an index past its vertices; a packed part outside its window.
static const uint PAST_VERTICES = 1;
static const uint OUTSIDE = 2;

// The 4-byte word at byte `at` of a packed index buffer's window, if it
// lies in it.
bool word(uint at, out uint value) {
    value = 0;
    if (limit < 4 || at > limit - 4) {
        return false;
    }
    value = source.Load(offset + at);
    return true;
}

// Index `i` of a meshlet-packed buffer: a 12-byte header (meshlets in the
// low 17 bits of its first word, the triangle words in its second), the
// meshlets (16 bytes each, their first vertex entry), one word per
// triangle (three 7-bit corners in the meshlet, the meshlet in its top 11
// bits), then the 16-bit vertex indices.
uint packed_index(uint i, out bool inside) {
    uint corner = i % 3;
    if (flip != 0 && corner != 1) {
        corner = 2 - corner;
    }
    uint h0, h1, t, m0, m1, w;
    inside = word(0, h0);
    inside = word(4, h1) && inside;
    uint triangles = 12 + 16 * (h0 & 0x1ffff);
    inside = word(triangles + 4 * (i / 3), t) && inside;
    uint meshlet = 12 + 16 * (t >> 21);
    inside = word(meshlet, m0) && inside;
    inside = word(meshlet + 4, m1) && inside;
    uint entry = (((m0 >> 14) & 1) << 16 | (m1 & 0xffff)) + ((t >> (7 * corner)) & 0x7f);
    uint at = triangles + 4 * h1 + 2 * entry;
    inside = word(at & ~3u, w) && inside;
    return (at & 2) != 0 ? w >> 16 : w & 0xffff;
}

// Vertex `i` of quantized vertices: the window starts at their 32-byte
// header (an origin, the axis order, a scale), each vertex 8 bytes of
// fixed-point coordinates (24, 24 and 16 bits).
float3 quantized(uint i) {
    float3 origin = asfloat(source.Load3(offset));
    uint axes = source.Load(offset + 12);
    float3 scale = asfloat(source.Load3(offset + 16));
    uint at = offset + 32 + i * stride;
    uint lo = source.Load(at);
    uint hi = source.Load(at + 4);
    float a = mad(scale.x, (float)(lo >> 8), origin.x);
    float b = mad(scale.y, (float)((lo & 0xff) | (hi & 0xffff) << 8), origin.y);
    float c = mad(scale.z, (float)(hi >> 16), origin.z);
    if (axes == 0) {
        return float3(c, b, a);
    }
    if (axes == 1) {
        return float3(b, c, a);
    }
    return float3(a, b, c);
}

[RootSignature("RootConstants(num32BitConstants=9, b0), UAV(u0), UAV(u1)")]
[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint i = thread.x + thread.y * (65535 * 64);
    if (step == 0) {
        if (i == 0) built.Store(0, 0);
        return;
    }
    if (i >= count) return;
    if (kind == 3) {
        if (step == 1) built.Store3(first + 12 * i, asuint(quantized(i)));
        return;
    }
    if (kind == 4) {
        if (step == 1) built.Store(first + 4 * i, source.Load(offset + 4 * i));
        return;
    }
    uint at = first + 4 * i;
    if (step == 1) {
        uint index;
        uint raised = 0;
        if (kind == 1) {
            index = source.Load(offset + 4 * i);
        } else if (kind == 2) {
            bool inside;
            index = packed_index(i, inside);
            if (!inside) raised = OUTSIDE;
        } else {
            uint b = offset + 2 * i;
            uint w = source.Load(b & ~3u);
            index = (b & 2) != 0 ? w >> 16 : w & 0xffff;
        }
        if (raised == 0 && index >= vertices) raised = PAST_VERTICES;
        if (raised != 0) {
            uint was;
            built.InterlockedOr(0, raised, was);
        }
        built.Store(at, index);
    } else if (built.Load(0) != 0) {
        built.Store(at, 0);
    }
}
