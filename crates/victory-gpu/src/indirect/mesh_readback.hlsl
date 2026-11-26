struct Vertex {
    float4 position : SV_Position;
};

[outputtopology("triangle")]
[numthreads(1, 1, 1)]
void mesh_main(out vertices Vertex points[3], out indices uint3 triangles[1]) {
    SetMeshOutputCounts(3, 1);
    points[0].position = float4(-0.75, 0.75, 0.5, 1.0);
    points[1].position = float4(0.75, 0.75, 0.5, 1.0);
    points[2].position = float4(-0.75, -0.75, 0.5, 1.0);
    triangles[0] = uint3(0, 1, 2);
}

float4 pixel_main() : SV_Target {
    return float4(0.0, 1.0, 0.0, 1.0);
}
