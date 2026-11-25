RaytracingAccelerationStructure scene : register(t0);
RWByteAddressBuffer result : register(u0);

[RootSignature("SRV(t0), UAV(u0)")]
[numthreads(2, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    RayDesc ray;
    ray.Origin = float3(id.x * 4, 0, -2);
    ray.Direction = float3(0, 0, 1);
    ray.TMin = 0;
    ray.TMax = 8;
    RayQuery<RAY_FLAG_FORCE_OPAQUE> query;
    query.TraceRayInline(scene, RAY_FLAG_NONE, 255, ray);
    while (query.Proceed()) {}
    bool hit = query.CommittedStatus() == COMMITTED_TRIANGLE_HIT;
    result.Store2(8 * id.x, uint2(hit, hit ? asuint(query.CommittedRayT()) : 0));
}
