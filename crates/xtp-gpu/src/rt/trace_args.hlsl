// The commands of a trace dispatch whose group counts the GPU wrote. The
// command count leads (bytes 0 to 3). Each 24-byte command from byte 8
// holds the TLAS root SRV resolved at submission (an empty TLAS when none
// was published: every ray misses), then the title's dispatch, clamped to
// D3D12's 65535 groups per dimension.
cbuffer Shape : register(b0) { uint records; uint stride; uint counted; }
ByteAddressBuffer tlas : register(t0);
RWByteAddressBuffer arguments : register(u0);
RWByteAddressBuffer count : register(u1);
RWByteAddressBuffer commands : register(u2);

[RootSignature("RootConstants(num32BitConstants=3,b0), SRV(t0), UAV(u0), UAV(u1), UAV(u2)")]
[numthreads(64, 1, 1)]
void main(uint3 thread : SV_DispatchThreadID) {
    uint i = thread.x;
    if (i == 0) commands.Store(0, counted != 0 ? min(count.Load(0), records) : records);
    if (i >= records) return;
    uint at = 8 + i * 24;
    commands.Store2(at, tlas.Load2(0));
    commands.Store3(at + 8, min(arguments.Load3(i * stride), 65535u));
    commands.Store(at + 20, 0);
}
