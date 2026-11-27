
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint compare;  // 0: the hashes are stored; else compared with those stored (2: and then stored)
};
ByteAddressBuffer now : register(t0);
// 16 bytes per piece of the copy: the hash of its bytes when they were
// last sent up (or hashed whole, the first time).
RWByteAddressBuffer hashes : register(u0);
RWByteAddressBuffer differs : register(u1);
static const uint4 seed = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu);
uint4 mix(uint4 h, uint4 words) {
    h = (h ^ words) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
    return h.yzwx ^ (h >> 15);
}
void finish(uint piece, uint index, uint4 h) {
    if (compare != 0u) {
        differs.Store(out_at + index * 4u, any(h != hashes.Load4(piece * 16u)) ? 1u : 0u);
        // With the copy-back on its own thread: the next compare of the
        // piece finds what is written from here on.
        if (compare == 2u) hashes.Store4(piece * 16u, h);
    } else {
        hashes.Store4(piece * 16u, h);
    }
}

// Each group owns one piece. Hash interleaved 16-byte words, then hash the
// 64 lane digests in lane order. Reordering words or lanes changes the digest.
groupshared uint4 partial[64];
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint lane : SV_GroupIndex) {
    for (uint index = group.x; index < pieces; index += 65535u) {
        uint piece = first + index;
        uint lo = piece * 65536u;
        lo = lo > skew ? lo - skew : 0u;
        uint hi = min((piece + 1u) * 65536u - skew, size);
        lo &= ~15u;
        hi = (hi + 15u) & ~15u;
        uint4 h = seed;
        for (uint at = lo + lane * 16u; at >= lo && at < hi; at += 1024u) {
            h = mix(h, now.Load4(at));
        }
        partial[lane] = h;
        GroupMemoryBarrierWithGroupSync();
        if (lane == 0u) {
            uint4 digest = seed;
            [loop]
            for (uint i = 0; i < 64; ++i) digest = mix(digest, partial[i]);
            finish(piece, index, digest);
        }
        GroupMemoryBarrierWithGroupSync();
    }
}
