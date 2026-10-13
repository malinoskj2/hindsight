// Tone-maps the scRGB picture of a display in HDR to 8-bit sRGB for the encoder: one triangle
// covering the target, one texel read per pixel. Compiled by build.rs with the Windows SDK's
// fxc.exe (`fullscreen` as vs_5_0, `tonemap` as ps_5_0) and included in the program; the
// maths is the same as `Curve::apply` in tonemap.rs, which the unit tests compare it with.

Texture2D<float4> scrgb : register(t0);

cbuffer Curve : register(b0)
{
    // scRGB to units of SDR white: 80 nits over the SDR white level.
    float scale;
    // The level, in units of SDR white, above which highlights are compressed.
    float knee;
    // How strongly they are: chosen so that the display's peak lands on 1.0.
    float rolloff;
    float unused;
};

float4 fullscreen(uint id : SV_VertexID) : SV_Position
{
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
}

float4 tonemap(float4 position : SV_Position) : SV_Target
{
    // Colours outside BT.709 come as negative components; they are clipped to it.
    float3 rgb = max(scrgb.Load(int3(position.xy, 0)).rgb * scale, 0);
    // The roll-off scales all three components by what it does to the largest, which keeps
    // a bright colour's hue instead of bleaching it.
    float largest = max(rgb.r, max(rgb.g, rgb.b));
    if (largest > knee)
    {
        float above = largest - knee;
        rgb *= (knee + above / (1 + above * rolloff)) / largest;
    }
    rgb = saturate(rgb);
    float3 encoded = rgb <= 0.0031308 ? rgb * 12.92 : 1.055 * pow(rgb, 1 / 2.4) - 0.055;
    return float4(encoded, 1);
}
