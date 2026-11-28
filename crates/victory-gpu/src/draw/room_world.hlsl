// Diffuse transfer from calibrated, one-sided rectangular emitters in game world units.
Texture2D<float4> Scene : register(t0);
Texture2D<float4> Material : register(t1);
Texture2D<float4> Normal : register(t2);
Texture2D<float> Depth : register(t3);
Texture2D<uint2> Stencil : register(t4);
RWTexture2D<float4> Result : register(u0);

struct Emitter {
    float4 center, halfU, halfV, radiance, roomMin, roomMax;
};
StructuredBuffer<Emitter> Emitters : register(t5);
cbuffer Camera : register(b0) { float4 basisX, basisY, basisZ, projection, originCount; };
static const float3 luma = float3(.2126,.7152,.0722);

float3 decodeSurfaceNormal(uint2 p) {
    uint3 bytes = (uint3)(Normal.Load(int3(p,0)).gba * 255.0 + 0.5);
    uint packed = bytes.x | (bytes.y << 8) | (bytes.z << 16);
    float2 oct = float2(packed & 4095u, (packed >> 12) & 4095u) * (2.0/4095.0) - 1.0;
    float3 n = float3(oct, 1.0 - abs(oct.x) - abs(oct.y));
    n.xy += float2(n.x >= 0 ? -1 : 1, n.y >= 0 ? -1 : 1) * saturate(-n.z);
    return normalize(n);
}
float3 toView(float3 delta) {
    return delta.x*basisX.xyz + delta.y*basisY.xyz + delta.z*basisZ.xyz;
}
float3 toWorld(float3 viewPosition) {
    return originCount.xyz + float3(dot(viewPosition,basisX.xyz),dot(viewPosition,basisY.xyz),dot(viewPosition,basisZ.xyz));
}
bool blockedOnScreen(float3 start, float3 end, uint2 dims) {
    [unroll] for (uint k=1; k<=3; ++k) {
        float3 samplePosition = lerp(start,end,k*0.25);
        if (samplePosition.z >= -0.01) continue;
        float2 uv = 0.5 + 0.5 * samplePosition.xy * projection.xy / -samplePosition.z * float2(1,-1);
        if (any(uv<0.0) || any(uv>=1.0)) continue;
        uint2 px = (uint2)(uv*float2(dims));
        float depth = Depth.Load(int3(px,0));
        float z = projection.w/max(depth+projection.z,1e-7);
        if (depth>0.0 && z+0.3 < -samplePosition.z) return true;
    }
    return false;
}
[RootSignature("DescriptorTable(SRV(t0,numDescriptors=5),UAV(u0)),RootConstants(num32BitConstants=20,b0),SRV(t5)")]
[numthreads(16,16,1)]
void diffuseWorld(uint3 thread : SV_DispatchThreadID) {
    uint2 dims; Scene.GetDimensions(dims.x,dims.y);
    uint2 p = thread.xy;
    if (any(p>=dims)) return;
    float4 original = Scene.Load(int3(p,0));
    float depth = Depth.Load(int3(p,0));
    if ((Stencil.Load(int3(p,0)).y & 24u) != 8u || depth<=0.0 || dot(original.rgb,luma)>2.0) {
        Result[p]=original;
        return;
    }
    float z = projection.w / max(depth+projection.z,1e-7);
    float2 ndc = (2.0*(float2(p)+0.5)/float2(dims)-1.0)*float2(1,-1);
    float3 view = float3(ndc*z/projection.xy,-z);
    float3 position = toWorld(view), normal = decodeSurfaceNormal(p);
    float3 irradiance=0;
    [loop] for(uint i=0; i<(uint)originCount.w; ++i) {
        Emitter e=Emitters[i];
        if(any(position<e.roomMin.xyz) || any(position>=e.roomMax.xyz)) continue;
        float3 orientedArea=cross(e.halfU.xyz,e.halfV.xyz);
        float area=length(orientedArea);
        float3 sourceNormal=orientedArea/max(area,1e-10);
        // Four Gauss-Legendre samples, each weighting one quarter of the area.
        [unroll] for (uint j=0; j<4; ++j) {
            float2 corner=float2((j&1u)==0u ? -1:1,(j&2u)==0u ? -1:1)*0.5773502692;
            float3 lamp=e.center.xyz+corner.x*e.halfU.xyz+corner.y*e.halfV.xyz;
            float3 delta=lamp-position;
            float distance2=dot(delta,delta);
            if (distance2 < .0001) continue;
            float3 direction=delta*rsqrt(distance2);
            float cosine=saturate(dot(normal,direction))*saturate(dot(sourceNormal,-direction));
            if (cosine>0 && !blockedOnScreen(view,toView(lamp-originCount.xyz),dims)) {
                irradiance+=e.radiance.xyz*area*cosine/(3.14159265*max(distance2,.01));
            }
        }
    }
    float3 albedo=saturate(Material.Load(int3(p,0)).rgb);
    Result[p]=float4(original.rgb+irradiance*albedo,original.a);
}
