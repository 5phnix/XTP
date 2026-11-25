// Rays along +z from z = -10 at the (x, y) of RAYS; each writes whether it
// hit and its distance. The known-bounds triangle (-5, 0, 1), (4, 0, 1),
// (4, 8, 9) lies in the plane z = 1 + y: a hit is at t = 11 + y. The first
// four rays fall inside it, near its edges; the last four just outside its
// x, hypotenuse, y and x edges.
RaytracingAccelerationStructure scene : register(t0);
RWByteAddressBuffer result : register(u0);

static const float2 RAYS[8] = {
    float2(0, 1), float2(3.5, 7), float2(-4, 0.5), float2(-4.9, 0.05),
    float2(4.5, 1), float2(-4, 2), float2(0, -0.5), float2(-5.5, 0.1),
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
    bool hit = query.CommittedStatus() == COMMITTED_TRIANGLE_HIT;
    result.Store2(8 * id.x, uint2(hit, hit ? asuint(query.CommittedRayT()) : 0));
}
