// Texel decoding and statistics shared by xtp-pix (PIX) and xtp-rdoc
// (RenderDoc): DXGI formats to linear floats, and the mean and max of the
// luminance and of each channel, with the counts of zero, NaN and infinite
// texels.
#pragma once
#include <dxgiformat.h>

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <cstring>

struct Stats {
    uint64_t n = 0, zero = 0, nan = 0, inf = 0;
    double sum_lum = 0, max_lum = -INFINITY;
    double sum[4] = {}, max[4] = {-INFINITY, -INFINITY, -INFINITY, -INFINITY};
    int channels = 0;
};

static float half_to_float(uint16_t h) {
    uint32_t s = (h >> 15) & 1, e = (h >> 10) & 31, m = h & 1023;
    float v;
    if (e == 0) v = std::ldexp((float)m, -24);
    else if (e == 31) v = m ? NAN : INFINITY;
    else v = std::ldexp((float)(m | 1024), (int)e - 25);
    return s ? -v : v;
}

// An unsigned float of e exponent bits (5) and m mantissa bits (R11G11B10).
static float small_float(uint32_t bits, int mbits) {
    uint32_t e = bits >> mbits, m = bits & ((1u << mbits) - 1);
    if (e == 0) return std::ldexp((float)m, -14 - mbits);
    if (e == 31) return m ? NAN : INFINITY;
    return std::ldexp((float)(m | (1u << mbits)), (int)e - 15 - mbits);
}

static float srgb_to_linear(float c) {
    return c <= 0.04045f ? c / 12.92f : std::pow((c + 0.055f) / 1.055f, 2.4f);
}

// Texel size in bytes and channel count of the formats xtp-pix reads; 0 for
// the others.
static int texel_bytes(DXGI_FORMAT f, int& channels) {
    switch (f) {
    case DXGI_FORMAT_R32G32B32A32_FLOAT: case DXGI_FORMAT_R32G32B32A32_UINT: channels = 4; return 16;
    case DXGI_FORMAT_R32G32B32_FLOAT: channels = 3; return 12;
    case DXGI_FORMAT_R16G16B16A16_FLOAT: case DXGI_FORMAT_R16G16B16A16_UNORM: case DXGI_FORMAT_R16G16B16A16_UINT:
        channels = 4; return 8;
    case DXGI_FORMAT_R32G32_FLOAT: case DXGI_FORMAT_R32G32_UINT: channels = 2; return 8;
    case DXGI_FORMAT_R10G10B10A2_UNORM: case DXGI_FORMAT_R10G10B10A2_UINT: channels = 4; return 4;
    case DXGI_FORMAT_R11G11B10_FLOAT: channels = 3; return 4;
    case DXGI_FORMAT_R9G9B9E5_SHAREDEXP: channels = 3; return 4;
    case DXGI_FORMAT_R8G8B8A8_UNORM: case DXGI_FORMAT_R8G8B8A8_UNORM_SRGB: case DXGI_FORMAT_R8G8B8A8_UINT:
    case DXGI_FORMAT_R8G8B8A8_SNORM: case DXGI_FORMAT_B8G8R8A8_UNORM: case DXGI_FORMAT_B8G8R8A8_UNORM_SRGB:
    case DXGI_FORMAT_B8G8R8X8_UNORM:
        channels = 4; return 4;
    case DXGI_FORMAT_R16G16_FLOAT: case DXGI_FORMAT_R16G16_UNORM: case DXGI_FORMAT_R16G16_UINT:
    case DXGI_FORMAT_R16G16_SNORM:
        channels = 2; return 4;
    case DXGI_FORMAT_R32_FLOAT: case DXGI_FORMAT_R32_UINT: case DXGI_FORMAT_D32_FLOAT:
    case DXGI_FORMAT_R24_UNORM_X8_TYPELESS: case DXGI_FORMAT_D24_UNORM_S8_UINT:
        channels = 1; return 4;
    case DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS: case DXGI_FORMAT_D32_FLOAT_S8X24_UINT: channels = 1; return 8;
    case DXGI_FORMAT_R8G8_UNORM: case DXGI_FORMAT_R8G8_UINT: channels = 2; return 2;
    case DXGI_FORMAT_R16_FLOAT: case DXGI_FORMAT_R16_UNORM: case DXGI_FORMAT_R16_UINT: case DXGI_FORMAT_D16_UNORM:
        channels = 1; return 2;
    case DXGI_FORMAT_R8_UNORM: case DXGI_FORMAT_R8_UINT: case DXGI_FORMAT_A8_UNORM: channels = 1; return 1;
    default: channels = 0; return 0;
    }
}

static void decode(DXGI_FORMAT f, const uint8_t* p, float v[4]) {
    v[0] = v[1] = v[2] = v[3] = 0;
    auto u8 = [&](int i) { return p[i] / 255.0f; };
    auto u16 = [&](int i) { uint16_t x; memcpy(&x, p + 2 * i, 2); return x; };
    auto u32 = [&](int i) { uint32_t x; memcpy(&x, p + 4 * i, 4); return x; };
    auto f32 = [&](int i) { float x; memcpy(&x, p + 4 * i, 4); return x; };
    switch (f) {
    case DXGI_FORMAT_R32G32B32A32_FLOAT: for (int i = 0; i < 4; i++) v[i] = f32(i); break;
    case DXGI_FORMAT_R32G32B32A32_UINT: for (int i = 0; i < 4; i++) v[i] = (float)u32(i); break;
    case DXGI_FORMAT_R32G32B32_FLOAT: for (int i = 0; i < 3; i++) v[i] = f32(i); break;
    case DXGI_FORMAT_R16G16B16A16_FLOAT: for (int i = 0; i < 4; i++) v[i] = half_to_float(u16(i)); break;
    case DXGI_FORMAT_R16G16B16A16_UNORM: for (int i = 0; i < 4; i++) v[i] = u16(i) / 65535.0f; break;
    case DXGI_FORMAT_R16G16B16A16_UINT: for (int i = 0; i < 4; i++) v[i] = (float)u16(i); break;
    case DXGI_FORMAT_R32G32_FLOAT: v[0] = f32(0); v[1] = f32(1); break;
    case DXGI_FORMAT_R32G32_UINT: v[0] = (float)u32(0); v[1] = (float)u32(1); break;
    case DXGI_FORMAT_R10G10B10A2_UNORM: case DXGI_FORMAT_R10G10B10A2_UINT: {
        uint32_t x = u32(0);
        float k = f == DXGI_FORMAT_R10G10B10A2_UNORM ? 1.0f : 0.0f;
        float s10 = k ? 1023.0f : 1.0f, s2 = k ? 3.0f : 1.0f;
        v[0] = (x & 1023) / s10; v[1] = ((x >> 10) & 1023) / s10; v[2] = ((x >> 20) & 1023) / s10; v[3] = (x >> 30) / s2;
        break;
    }
    case DXGI_FORMAT_R11G11B10_FLOAT: {
        uint32_t x = u32(0);
        v[0] = small_float(x & 2047, 6); v[1] = small_float((x >> 11) & 2047, 6); v[2] = small_float(x >> 22, 5);
        break;
    }
    case DXGI_FORMAT_R9G9B9E5_SHAREDEXP: {
        uint32_t x = u32(0);
        float scale = std::ldexp(1.0f, (int)(x >> 27) - 15 - 9);
        v[0] = (x & 511) * scale; v[1] = ((x >> 9) & 511) * scale; v[2] = ((x >> 18) & 511) * scale;
        break;
    }
    case DXGI_FORMAT_R8G8B8A8_UNORM: case DXGI_FORMAT_R8G8B8A8_UINT: case DXGI_FORMAT_R8G8B8A8_UNORM_SRGB:
        for (int i = 0; i < 4; i++) v[i] = f == DXGI_FORMAT_R8G8B8A8_UINT ? (float)p[i] : u8(i);
        if (f == DXGI_FORMAT_R8G8B8A8_UNORM_SRGB) for (int i = 0; i < 3; i++) v[i] = srgb_to_linear(v[i]);
        break;
    case DXGI_FORMAT_R8G8B8A8_SNORM: for (int i = 0; i < 4; i++) v[i] = std::max(-1.0f, (int8_t)p[i] / 127.0f); break;
    case DXGI_FORMAT_B8G8R8A8_UNORM: case DXGI_FORMAT_B8G8R8A8_UNORM_SRGB: case DXGI_FORMAT_B8G8R8X8_UNORM:
        v[0] = u8(2); v[1] = u8(1); v[2] = u8(0); v[3] = f == DXGI_FORMAT_B8G8R8X8_UNORM ? 1.0f : u8(3);
        if (f == DXGI_FORMAT_B8G8R8A8_UNORM_SRGB) for (int i = 0; i < 3; i++) v[i] = srgb_to_linear(v[i]);
        break;
    case DXGI_FORMAT_R16G16_FLOAT: v[0] = half_to_float(u16(0)); v[1] = half_to_float(u16(1)); break;
    case DXGI_FORMAT_R16G16_UNORM: v[0] = u16(0) / 65535.0f; v[1] = u16(1) / 65535.0f; break;
    case DXGI_FORMAT_R16G16_SNORM: v[0] = (int16_t)u16(0) / 32767.0f; v[1] = (int16_t)u16(1) / 32767.0f; break;
    case DXGI_FORMAT_R16G16_UINT: v[0] = (float)u16(0); v[1] = (float)u16(1); break;
    case DXGI_FORMAT_R32_FLOAT: case DXGI_FORMAT_D32_FLOAT: case DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS:
    case DXGI_FORMAT_D32_FLOAT_S8X24_UINT:
        v[0] = f32(0); break;
    case DXGI_FORMAT_R32_UINT: v[0] = (float)u32(0); break;
    case DXGI_FORMAT_R24_UNORM_X8_TYPELESS: case DXGI_FORMAT_D24_UNORM_S8_UINT:
        v[0] = (u32(0) & 0xffffff) / 16777215.0f; break;
    case DXGI_FORMAT_R8G8_UNORM: v[0] = u8(0); v[1] = u8(1); break;
    case DXGI_FORMAT_R8G8_UINT: v[0] = p[0]; v[1] = p[1]; break;
    case DXGI_FORMAT_R16_FLOAT: v[0] = half_to_float(u16(0)); break;
    case DXGI_FORMAT_R16_UNORM: case DXGI_FORMAT_D16_UNORM: v[0] = u16(0) / 65535.0f; break;
    case DXGI_FORMAT_R16_UINT: v[0] = (float)u16(0); break;
    case DXGI_FORMAT_R8_UNORM: case DXGI_FORMAT_A8_UNORM: v[0] = u8(0); break;
    case DXGI_FORMAT_R8_UINT: v[0] = p[0]; break;
    default: break;
    }
}

// The format of a typeless texture's data, from the view's.
static DXGI_FORMAT pick_format(DXGI_FORMAT data, DXGI_FORMAT view) {
    int c;
    if (texel_bytes(data, c)) return data;
    if (texel_bytes(view, c)) return view;
    switch (data) {
    case DXGI_FORMAT_R16G16B16A16_TYPELESS: return DXGI_FORMAT_R16G16B16A16_FLOAT;
    case DXGI_FORMAT_R32G32B32A32_TYPELESS: return DXGI_FORMAT_R32G32B32A32_FLOAT;
    case DXGI_FORMAT_R8G8B8A8_TYPELESS: return DXGI_FORMAT_R8G8B8A8_UNORM;
    case DXGI_FORMAT_B8G8R8A8_TYPELESS: return DXGI_FORMAT_B8G8R8A8_UNORM;
    case DXGI_FORMAT_R10G10B10A2_TYPELESS: return DXGI_FORMAT_R10G10B10A2_UNORM;
    case DXGI_FORMAT_R32_TYPELESS: return DXGI_FORMAT_R32_FLOAT;
    case DXGI_FORMAT_R16_TYPELESS: return DXGI_FORMAT_R16_FLOAT;
    case DXGI_FORMAT_R8_TYPELESS: return DXGI_FORMAT_R8_UNORM;
    case DXGI_FORMAT_R16G16_TYPELESS: return DXGI_FORMAT_R16G16_FLOAT;
    case DXGI_FORMAT_R32G32_TYPELESS: return DXGI_FORMAT_R32G32_FLOAT;
    case DXGI_FORMAT_R24G8_TYPELESS: return DXGI_FORMAT_R24_UNORM_X8_TYPELESS;
    case DXGI_FORMAT_R32G8X24_TYPELESS: return DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS;
    default: return data;
    }
}

// The statistics of one subresource laid out in rows of `pitch` bytes from
// `offset` (depth slices one after the other).
static void measure_raw(DXGI_FORMAT format, const uint8_t* data, size_t size, uint32_t w, uint32_t h, uint32_t d,
                        size_t pitch, size_t offset, Stats& s) {
    int channels = 0;
    int texel = texel_bytes(format, channels);
    s.channels = channels;
    if (!texel) return;
    d = std::max<uint32_t>(d, 1);
    for (uint32_t z = 0; z < d; z++)
        for (uint32_t y = 0; y < h; y++) {
            size_t row = offset + ((size_t)z * h + y) * pitch;
            for (uint32_t x = 0; x < w; x++) {
                size_t at = row + (size_t)x * texel;
                if (at + texel > size) return;
                float v[4];
                decode(format, data + at, v);
                bool nan = false, inf = false, zero = true;
                for (int c = 0; c < channels; c++) {
                    if (std::isnan(v[c])) nan = true;
                    else if (std::isinf(v[c])) inf = true;
                    if (v[c] != 0) zero = false;
                }
                s.n++;
                if (nan) { s.nan++; continue; }
                if (inf) { s.inf++; continue; }
                if (zero) s.zero++;
                double lum = channels >= 3 ? 0.2126 * v[0] + 0.7152 * v[1] + 0.0722 * v[2] : v[0];
                s.sum_lum += lum;
                s.max_lum = std::max(s.max_lum, lum);
                for (int c = 0; c < channels; c++) {
                    s.sum[c] += v[c];
                    s.max[c] = std::max(s.max[c], (double)v[c]);
                }
            }
        }
}
