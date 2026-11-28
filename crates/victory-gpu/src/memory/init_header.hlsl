// XboxBvhBuilder 3cea4534 writes four vectors to each address in its
// 40-byte input records. Reconcile only that very header with its UAV.
ByteAddressBuffer jobs : register(t0);
RWByteAddressBuffer raw : register(u0);
RWByteAddressBuffer builder : register(u1);
cbuffer Inputs : register(b0) { uint lo, hi, count; }
[numthreads(1,1,1)]
void main() {
    for (uint i=0; i<count; ++i) {
        uint at=i*40;
        if (any(jobs.Load2(at)!=uint2(lo,hi))) continue;
        uint4 a=raw.Load4(0), b=raw.Load4(16), c=raw.Load4(32), d=raw.Load4(48);
        if (any(a!=uint4(jobs.Load(at+16),0,jobs.Load(at+20),jobs.Load(at+24)))) continue;
        if (any(b!=uint4(0x7f7fffff,0x7f7fffff,0x7f7fffff,0xff7fffff))) continue;
        if (any(c!=uint4(0xff7fffff,0xff7fffff,0,jobs.Load(at+28)))) continue;
        if (any(d!=uint4(0xffffffff,jobs.Load(at+32),jobs.Load(at+36),0x007f7f7f))) continue;
        builder.Store4(0,a); builder.Store4(16,b); builder.Store4(32,c); builder.Store4(48,d);
        break;
    }
}
