// Rays along +z from z = -10 at the (x, y) of RAYS, for the tests of the
// console builder's formats (rt/tests.rs, MESH): each writes whether it
// hit, its distance, whether the triangle it hit faces it, and that
// triangle's index in its geometry. The first five fall inside the mesh's
// triangles 0, 1, 2, 0 and 2, the last three just outside them.
RaytracingAccelerationStructure scene : register(t0);
RWByteAddressBuffer result : register(u0);

static const float2 RAYS[8] = {
    float2(0, 1), float2(20, 1), float2(40, 1), float2(3.5, 7),
    float2(43.5, 7), float2(-5.5, 0.1), float2(25, 1), float2(40, 7.9),
};

[RootSignature("SRV(t0), UAV(u0)")]
[numthreads(8, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    RayDesc ray;
    ray.Origin = float3(RAYS[id.x], -10);
    ray.Direction = float3(0, 0, 1);
    ray.TMin = 0;
    ray.TMax = 100;
    RayQuery<RAY_FLAG_FORCE_OPAQUE> query;
    query.TraceRayInline(scene, RAY_FLAG_NONE, 255, ray);
    while (query.Proceed()) {}
    uint4 read = 0;
    if (query.CommittedStatus() == COMMITTED_TRIANGLE_HIT) {
        read = uint4(1, asuint(query.CommittedRayT()), query.CommittedTriangleFrontFace(),
                     query.CommittedPrimitiveIndex());
    }
    result.Store4(16 * id.x, read);
}
