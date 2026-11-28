// dxc -T cs_6_6 -E main -Fo raw_fault_pages.dxil raw_fault_pages.hlsl
ByteAddressBuffer directory : register(t0);
RWByteAddressBuffer result : register(u0);
RWByteAddressBuffer faults : register(u1);
cbuffer Args : register(b0) { uint page; uint within; uint heapBase; uint outputOffset; };

[numthreads(1, 1, 1)]
void main() {
    uint2 entry = directory.Load2(page * 8);
    if (entry.x == 0)
        faults.Store(4 * (1 + page % 4095), page);
    RWByteAddressBuffer data = ResourceDescriptorHeap[heapBase + entry.x];
    result.Store(outputOffset, data.Load(entry.y + within));
}
