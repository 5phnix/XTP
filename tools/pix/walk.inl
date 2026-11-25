// xtp-pix walk: replays a capture and measures what its passes write (and,
// on request, read). Included at the end of xtp-pix.cpp.
//
//   xtp-pix walk <file.wpix> <out.tsv> [--queue ID] [--from N] [--to N]
//                [--each] [--inputs] [--accessed] [--depth-diff] [--max-views N]
//
// A pass is a run of consecutive draws or dispatches of one queue with the
// same program (XTP's marker before them, else the pipeline state) and the
// same outputs (render targets, depth, UAVs). By default each pass is
// measured once, after its last event; --each measures after every draw and
// dispatch in [--from, --to]. --inputs also measures, before the event,
// every texture it reads (SRVs, and the UAVs it may read). --accessed asks
// PIX for the resources each event really touched (one instrumented replay
// first): XTP's shaders index the descriptor heap directly, so without it
// only render targets and depth are known. --depth-diff counts, for each
// draw with a depth target, the depth texels that the draw changed.
//
// One row per measured view: the event, the pass, XTP's marker text, the
// view (kind, binding, resource name, size, format) and the statistics of
// the subresource the view selects: mean and max of the luminance
// (0.2126 R + 0.7152 G + 0.0722 B for three channels or more, else the
// first channel), mean and max of each channel, and the counts of texels
// that are zero in every channel, NaN or infinite.

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

struct Texels {
    PIX_TEXTURE_INFO info{};
    PIX_TEXTURE_SUBRESOURCE_INFO sub{};
    std::vector<uint8_t> bytes;
    DXGI_FORMAT format = DXGI_FORMAT_UNKNOWN;
};

static void measure(const Texels& t, Stats& s) {
    int channels = 0;
    int size = texel_bytes(t.format, channels);
    s.channels = channels;
    if (!size) return;
    const auto& fp = t.sub.PlacedSubresourceFootprint.Footprint;
    UINT w = fp.Width, h = fp.Height, d = std::max<UINT>(fp.Depth, 1);
    UINT pitch = fp.RowPitch;
    for (UINT z = 0; z < d; z++)
        for (UINT y = 0; y < h; y++) {
            size_t row = (size_t)t.sub.PlacedSubresourceFootprint.Offset + ((size_t)z * h + y) * pitch;
            for (UINT x = 0; x < w; x++) {
                size_t at = row + (size_t)x * size;
                if (at + size > t.bytes.size()) return;
                float v[4];
                decode(t.format, t.bytes.data() + at, v);
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

// How many texels differ between two reads of one subresource.
static uint64_t changed_texels(const Texels& a, const Texels& b) {
    int channels = 0;
    int size = texel_bytes(a.format, channels);
    if (!size || a.bytes.size() != b.bytes.size()) return UINT64_MAX;
    const auto& fp = a.sub.PlacedSubresourceFootprint.Footprint;
    uint64_t n = 0;
    for (UINT y = 0; y < fp.Height; y++) {
        size_t row = (size_t)a.sub.PlacedSubresourceFootprint.Offset + (size_t)y * fp.RowPitch;
        for (UINT x = 0; x < fp.Width; x++) {
            size_t at = row + (size_t)x * size;
            if (at + size > a.bytes.size()) break;
            // Depth only: the first 4 bytes of D32S8 / D24S8 texels.
            int cmp = std::min(size, 4);
            if (memcmp(a.bytes.data() + at, b.bytes.data() + at, cmp)) n++;
        }
    }
    return n;
}

struct View {
    PIX_RESOURCE_VIEW_TYPE type = PIX_RESOURCE_NONE;
    Com<IPixResourceView> view;
    Com<IPixD3D12Resource> resource;
    PIX_API_OBJECT_ID id{};
    std::string name, binding;
    DXGI_FORMAT view_format = DXGI_FORMAT_UNKNOWN;
    UINT subresource = 0;
    bool texture = false;
};

static const char* kind_name(PIX_RESOURCE_VIEW_TYPE t) {
    switch (t) {
    case PIX_RESOURCE_RENDER_TARGET_VIEW: return "RTV";
    case PIX_RESOURCE_DEPTH_STENCIL_VIEW: return "DSV";
    case PIX_RESOURCE_UNORDERED_ACCESS_VIEW: return "UAV";
    case PIX_RESOURCE_SHADER_RESOURCE_VIEW: return "SRV";
    case PIX_RESOURCE_CONSTANT_BUFFER_VIEW: return "CBV";
    case PIX_RESOURCE_VERTEX_BUFFER_VIEW: return "VBV";
    case PIX_RESOURCE_INDEX_BUFFER_VIEW: return "IBV";
    default: return "other";
    }
}

static std::string binding_text(IPixResourceView* v) {
    Com<IPixResourceViewBindings> bs;
    if (FAILED(v->GetBindings(__uuidof(IPixResourceViewBindings), bs.put_void())) || !bs) return "";
    std::string out;
    for (UINT32 i = 0; i < bs->GetCount() && i < 4; i++) {
        PIX_RESOURCE_BINDING_TYPE t;
        if (FAILED(bs->GetViewType(i, &t))) continue;
        char buf[128] = {};
        if (t == PIX_RESOURCE_BINDING_ROOT_PARAMETER_BOUND) {
            Com<IPixRootParameterResourceBinding> b;
            if (SUCCEEDED(bs->GetViewBinding(i, __uuidof(IPixRootParameterResourceBinding), b.put_void())) && b) {
                UINT32 heap = 0;
                if (FAILED(b->GetDescriptorHeapIndex(&heap))) heap = UINT32_MAX;
                snprintf(buf, sizeof buf, "root%u space%u reg%u heap%d", b->GetRootParameterIndex(), b->GetSpace(),
                         b->GetIndex(), heap == UINT32_MAX ? -1 : (int)heap);
            }
        } else if (t == PIX_RESOURCE_BINDLESS) {
            Com<IPixDynamicHeapResourceBinding> b;
            if (SUCCEEDED(bs->GetViewBinding(i, __uuidof(IPixDynamicHeapResourceBinding), b.put_void())) && b)
                snprintf(buf, sizeof buf, "heap[%u]", b->GetDescriptorHeapIndex());
        } else if (t == PIX_RESOURCE_BINDING_PROGRAM_BOUND) {
            Com<IPixProgramResourceBinding> b;
            if (SUCCEEDED(bs->GetViewBinding(i, __uuidof(IPixProgramResourceBinding), b.put_void())) && b)
                snprintf(buf, sizeof buf, "slot%u", b->GetBindingIndex());
        } else if (t == PIX_RESOURCE_BINDING_API_PARAMETER) {
            snprintf(buf, sizeof buf, "api");
        }
        if (*buf) {
            if (!out.empty()) out += ",";
            out += buf;
        }
    }
    return out;
}

// The views bound (and, with the analysis' accessed list, touched) at an event.
static void collect_views(IPixGpuCaptureDocument* doc, IPixGpuCaptureAnalysis* analysis, bool accessed,
                          const PIX_EVENT_INFO& e, std::vector<View>& out, Com<IPixPipelineState>* pso,
                          std::string* program) {
    out.clear();
    Com<IPixProgramState> ps;
    if (FAILED(doc->GetProgramState(&e, __uuidof(IPixProgramState), ps.put_void())) || !ps) return;
    Com<IPixGenericPipeline> gp;
    if (FAILED(ps->GetGpuProgram(__uuidof(IPixGenericPipeline), gp.put_void())) || !gp) return;
    if (pso) {
        Com<IPixPipelineState> p;
        if (SUCCEEDED(gp->GetPipelineState(__uuidof(IPixPipelineState), p.put_void())) && p) {
            if (program) {
                char buf[64];
                snprintf(buf, sizeof buf, "pso%llu", (unsigned long long)p->GetApiObjectId());
                *program = buf;
                std::string n = narrow(p->GetName());
                if (!n.empty()) *program += " " + n;
            }
            *pso = std::move(p);
        }
    }
    Com<IPixResourceViewsAtEvent> views;
    if (FAILED(gp->GetResourceViews(__uuidof(IPixResourceViewsAtEvent), views.put_void())) || !views) return;
    if (accessed && analysis) analysis->GetAccessedResources(views.p);
    std::set<std::pair<int, uint64_t>> seen;
    for (UINT32 i = 0; i < views->GetCount(); i++) {
        View v;
        if (FAILED(views->GetType(i, &v.type))) continue;
        if (v.type != PIX_RESOURCE_RENDER_TARGET_VIEW && v.type != PIX_RESOURCE_DEPTH_STENCIL_VIEW &&
            v.type != PIX_RESOURCE_UNORDERED_ACCESS_VIEW && v.type != PIX_RESOURCE_SHADER_RESOURCE_VIEW &&
            v.type != PIX_RESOURCE_CONSTANT_BUFFER_VIEW)
            continue;
        if (FAILED(views->GetView(i, __uuidof(IPixResourceView), v.view.put_void())) || !v.view) continue;
        Com<IPixD3D12ResourceView> rv;
        if (FAILED(v.view->QueryInterface(__uuidof(IPixD3D12ResourceView), rv.put_void())) || !rv) continue;
        if (FAILED(rv->GetD3D12Resource(__uuidof(IPixD3D12Resource), v.resource.put_void())) || !v.resource) continue;
        v.id = v.resource->GetApiObjectId();
        v.name = narrow(v.resource->GetName());
        v.binding = binding_text(v.view.p);
        // The subresource the view starts at, and its format.
        D3D12_RESOURCE_DIMENSION dim = D3D12_RESOURCE_DIMENSION_UNKNOWN;
        UINT16 mips = 1, arrays = 1;
        PIX_RESOURCE_DESC rd = v.resource->GetResourceDesc();
        if (rd.Type == PIX_RESOURCE_DESC_TYPE_DESC2 && rd.Desc2) {
            // D3D12_RESOURCE_DESC2 starts as D3D12_RESOURCE_DESC does.
            const auto* d = reinterpret_cast<const D3D12_RESOURCE_DESC*>(rd.Desc2);
            dim = d->Dimension;
            mips = std::max<UINT16>(d->MipLevels, 1);
            arrays = std::max<UINT16>(d->Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D ? 1 : d->DepthOrArraySize, 1);
        }
        v.texture = dim == D3D12_RESOURCE_DIMENSION_TEXTURE1D || dim == D3D12_RESOURCE_DIMENSION_TEXTURE2D ||
                    dim == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        Com<IPixTextureResourceView> tv;
        if (v.texture && SUCCEEDED(v.view->QueryInterface(__uuidof(IPixTextureResourceView), tv.put_void())) && tv) {
            D3D12_BARRIER_SUBRESOURCE_RANGE r{};
            if (SUCCEEDED(tv->GetSubresourceRange(&r))) {
                if (r.NumMipLevels == 0) v.subresource = r.IndexOrFirstMipLevel;
                else v.subresource = r.IndexOrFirstMipLevel + r.FirstArraySlice * mips + r.FirstPlane * mips * arrays;
            }
        }
        if (v.type == PIX_RESOURCE_RENDER_TARGET_VIEW) {
            Com<IPixRenderTargetView> x;
            if (SUCCEEDED(v.view->QueryInterface(__uuidof(IPixRenderTargetView), x.put_void())) && x && x->GetDesc())
                v.view_format = x->GetDesc()->Format;
        } else if (v.type == PIX_RESOURCE_DEPTH_STENCIL_VIEW) {
            Com<IPixDepthStencilView> x;
            if (SUCCEEDED(v.view->QueryInterface(__uuidof(IPixDepthStencilView), x.put_void())) && x && x->GetDesc())
                v.view_format = x->GetDesc()->Format;
        } else if (v.type == PIX_RESOURCE_UNORDERED_ACCESS_VIEW) {
            Com<IPixUnorderedAccessView> x;
            const D3D12_UNORDERED_ACCESS_VIEW_DESC* d = nullptr;
            if (SUCCEEDED(v.view->QueryInterface(__uuidof(IPixUnorderedAccessView), x.put_void())) && x &&
                SUCCEEDED(x->GetDesc(&d)) && d)
                v.view_format = d->Format;
        } else if (v.type == PIX_RESOURCE_SHADER_RESOURCE_VIEW) {
            Com<IPixShaderResourceView> x;
            const D3D12_SHADER_RESOURCE_VIEW_DESC* d = nullptr;
            if (SUCCEEDED(v.view->QueryInterface(__uuidof(IPixShaderResourceView), x.put_void())) && x &&
                SUCCEEDED(x->GetDesc(&d)) && d)
                v.view_format = d->Format;
        }
        if (!seen.insert({(int)v.type * 1000003 + (int)v.subresource, (uint64_t)v.id}).second) continue;
        out.push_back(std::move(v));
    }
}

static bool read_view(IPixGpuCaptureAnalysis* a, const View& v, const PIX_EVENT_INFO& e, bool before, Texels& t,
                      HRESULT& hr) {
    Com<IPixD3D12ResourceView> rv;
    if (FAILED(hr = v.view->QueryInterface(__uuidof(IPixD3D12ResourceView), rv.put_void()))) return false;
    PIX_GET_TEXTURE_BYTES_PARAMETERS p{before, FALSE, v.subresource, 0};
    Com<IPixTextureSubresource> sub;
    hr = a->GetTextureBytesFromResourceView(rv.p, &e, &p, __uuidof(IPixTextureSubresource), sub.put_void());
    if (FAILED(hr) || !sub) return false;
    t.info = sub->GetInfo();
    t.sub = sub->GetSubresource();
    t.bytes.resize((size_t)sub->GetSizeBytes());
    if (FAILED(hr = sub->GetData(t.bytes.size(), t.bytes.data()))) return false;
    t.format = pick_format(t.info.TextureFormat, v.view_format);
    return true;
}

// Depth enable and write mask from a normalized pipeline state stream.
static std::string depth_state(IPixPipelineState* pso) {
    if (!pso) return "";
    const PIX_PIPELINE_STATE_STREAM_DESC* d = pso->GetDesc();
    if (!d || !d->SubobjectStream) return "";
    const uint8_t* start = static_cast<const uint8_t*>(d->SubobjectStream);
    const uint8_t* p = start;
    const uint8_t* end = p + d->SizeInBytes;
    auto align = [](size_t x, size_t a) { return (x + a - 1) / a * a; };
    std::string dsv_format;
    std::string out;
    while (p + 4 <= end) {
        auto type = *reinterpret_cast<const D3D12_PIPELINE_STATE_SUBOBJECT_TYPE*>(p);
        size_t inner_align = 4, inner = 0;
        switch (type) {
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE: inner_align = 8; inner = 8; break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS: case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS:
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS: case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS:
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS: case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS:
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS: case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS:
            inner_align = 8; inner = sizeof(D3D12_SHADER_BYTECODE); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_STREAM_OUTPUT: inner_align = 8; inner = sizeof(D3D12_STREAM_OUTPUT_DESC); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND: inner = sizeof(D3D12_BLEND_DESC); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK: inner = 4; break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER: inner = sizeof(D3D12_RASTERIZER_DESC); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1: inner = sizeof(D3D12_RASTERIZER_DESC1); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2: inner = sizeof(D3D12_RASTERIZER_DESC2); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL: inner = sizeof(D3D12_DEPTH_STENCIL_DESC); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1: inner = sizeof(D3D12_DEPTH_STENCIL_DESC1); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2: inner = sizeof(D3D12_DEPTH_STENCIL_DESC2); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT: inner_align = 8; inner = sizeof(D3D12_INPUT_LAYOUT_DESC); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE: case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY:
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT: case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_NODE_MASK:
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_FLAGS:
            inner = 4; break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS: inner = sizeof(D3D12_RT_FORMAT_ARRAY); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC: inner = sizeof(DXGI_SAMPLE_DESC); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO: inner_align = 8; inner = sizeof(D3D12_CACHED_PIPELINE_STATE); break;
        case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING: inner_align = 8; inner = sizeof(D3D12_VIEW_INSTANCING_DESC); break;
        default: return out.empty() ? "stream?" : out;
        }
        const uint8_t* body = p + align(4, inner_align);
        if (body + inner > end) break;
        char buf[96] = {};
        if (type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL || type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1) {
            auto* ds = reinterpret_cast<const D3D12_DEPTH_STENCIL_DESC*>(body);
            snprintf(buf, sizeof buf, "depth %s write %s func %d", ds->DepthEnable ? "on" : "off",
                     ds->DepthWriteMask ? "on" : "off", (int)ds->DepthFunc);
        } else if (type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2) {
            auto* ds = reinterpret_cast<const D3D12_DEPTH_STENCIL_DESC2*>(body);
            snprintf(buf, sizeof buf, "depth %s write %s func %d", ds->DepthEnable ? "on" : "off",
                     ds->DepthWriteMask ? "on" : "off", (int)ds->DepthFunc);
        } else if (type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER || type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1 ||
                   type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2) {
            // FillMode and CullMode lead all three.
            auto* r = reinterpret_cast<const D3D12_RASTERIZER_DESC*>(body);
            snprintf(buf, sizeof buf, "cull %d", (int)r->CullMode);
        } else if (type == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND) {
            auto* b = reinterpret_cast<const D3D12_BLEND_DESC*>(body);
            snprintf(buf, sizeof buf, "blend0 %s mask %x", b->RenderTarget[0].BlendEnable ? "on" : "off",
                     b->RenderTarget[0].RenderTargetWriteMask);
        }
        if (*buf) {
            if (!out.empty()) out += "; ";
            out += buf;
        }
        // Subobjects are pointer-aligned from the stream's start.
        p = start + align((size_t)(body + inner - start), sizeof(void*));
    }
    return out;
}

static bool is_work(const std::string& n) {
    return n.rfind("Draw", 0) == 0 || n.rfind("Dispatch", 0) == 0 || n.rfind("ExecuteIndirect", 0) == 0;
}

// XTP's marker before a draw or dispatch: "draw ...", "indexed ...",
// "dispatch ...", "mesh ..." (crates/xtp-gpu/src/draw.rs, op_text).
static bool xtp_marker(const std::string& s) {
    return s.rfind("draw ", 0) == 0 || s.rfind("indexed ", 0) == 0 || s.rfind("dispatch ", 0) == 0 ||
           s.rfind("mesh ", 0) == 0;
}

// The program hashes of a marker ("| <hash>:" per stage).
static std::string marker_programs(const std::string& m) {
    std::string out;
    for (size_t at = m.find("| "); at != std::string::npos; at = m.find("| ", at + 2)) {
        size_t colon = m.find(':', at);
        if (colon == std::string::npos) break;
        if (!out.empty()) out += "+";
        out += m.substr(at + 2, colon - at - 2);
    }
    return out;
}

static std::string trim(const std::string& s, size_t n) { return s.size() > n ? s.substr(0, n) + "..." : s; }

static int walk(int argc, wchar_t** argv) {
    if (argc < 2) {
        logf("walk <file.wpix> <out.tsv> [--queue ID] [--from N] [--to N] [--each] [--inputs] [--accessed] [--depth-diff]");
        return 2;
    }
    std::wstring file = argv[0], outp = argv[1];
    int queue_id = -1;
    UINT32 from = 0, to = UINT32_MAX;
    bool each = false, inputs = false, accessed = false, depth_diff = false;
    size_t max_views = 64;
    for (int i = 2; i < argc; i++) {
        std::wstring a = argv[i];
        auto next = [&]() -> std::wstring { return i + 1 < argc ? argv[++i] : L"0"; };
        if (a == L"--queue") queue_id = _wtoi(next().c_str());
        else if (a == L"--from") from = (UINT32)_wtoi(next().c_str());
        else if (a == L"--to") to = (UINT32)_wtoi(next().c_str());
        else if (a == L"--each") each = true;
        else if (a == L"--inputs") inputs = true;
        else if (a == L"--accessed") accessed = true;
        else if (a == L"--depth-diff") depth_diff = true;
        else if (a == L"--max-views") max_views = (size_t)_wtoi(next().c_str());
        else if (a == L"--log") _wfopen_s(&g_log, next().c_str(), L"a");
        else {
            logf("walk: unknown argument %s", narrow(a.c_str()).c_str());
            return 2;
        }
    }
    Com<IPixFactory> factory;
    Com<IPixGpuCaptureDocument> doc;
    if (open_capture(file, factory, doc)) return 1;
    std::vector<Queue> qs;
    if (queues(doc.p, qs)) return 1;

    Com<IPixGpuCaptureAnalysis> analysis;
    CHECK(doc->GetAnalysis(__uuidof(IPixGpuCaptureAnalysis), analysis.put_void()));
    PIX_CONNECTION_DESC_LOCAL local{PIX_CONNECTION_FLAG_LOCAL_NONE};
    PIX_CONNECTION_DESC cdesc{};
    cdesc.Type = PIX_CONNECTION_TYPE_LOCAL;
    cdesc.pLocal = &local;
    CHECK(analysis->Connect(&cdesc, nullptr));
    PIX_ANALYSIS_PARAMS params{PIX_ADAPTER_ID_DEFAULT, PIX_ANALYSIS_FLAG_NONE};
    auto* anotes = new AnalysisNotes();
    logf("starting the analysis (replay)");
    CHECK(analysis->StartAnalysis(&params, anotes, nullptr));
    if (accessed) {
        logf("gathering the accessed resources (instrumented replay)");
        CHECK(analysis->GatherAccessedResources());
    }

    FILE* out = nullptr;
    if (_wfopen_s(&out, outp.c_str(), L"w") || !out) {
        logf("cannot write %s", narrow(outp.c_str()).c_str());
        return 1;
    }
    fprintf(out,
            "queue\tevent\tpass\tcall\tprograms\twhen\tkind\tbinding\tresource\tid\tsize\tformat\tmean_lum\tmax_lum\t"
            "mean_r\tmean_g\tmean_b\tmean_a\tmax_r\tmax_g\tmax_b\tmax_a\tzero\tnan\tinf\ttexels\tnote\n");

    auto row = [&](const Queue& q, const PIX_EVENT_INFO& e, int pass, const std::string& call,
                   const std::string& programs, const char* when, const View& v, const Texels* t, const Stats* s,
                   const std::string& note) {
        char size[64] = "", fmt[16] = "";
        if (t) {
            const auto& fp = t->sub.PlacedSubresourceFootprint.Footprint;
            snprintf(size, sizeof size, "%ux%ux%u", fp.Width, fp.Height, fp.Depth);
            snprintf(fmt, sizeof fmt, "%d", (int)t->format);
        }
        fprintf(out, "%u\t%u\t%d\t%s\t%s\t%s\t%s\t%s\t%s\t%llu\t%s\t%s", q.id, e.Index, pass, field(call.c_str()).c_str(),
                field(programs.c_str()).c_str(), when, kind_name(v.type), field(v.binding.c_str()).c_str(),
                field(v.name.c_str()).c_str(), (unsigned long long)v.id, size, fmt);
        if (s && s->n && s->channels) {
            uint64_t good = s->n - s->nan - s->inf;
            double g = good ? (double)good : 1.0;
            fprintf(out, "\t%.6g\t%.6g", s->sum_lum / g, good ? s->max_lum : 0.0);
            for (int c = 0; c < 4; c++) fprintf(out, "\t%.6g", c < s->channels ? s->sum[c] / g : 0.0);
            for (int c = 0; c < 4; c++) fprintf(out, "\t%.6g", c < s->channels && good ? s->max[c] : 0.0);
            fprintf(out, "\t%llu\t%llu\t%llu\t%llu", (unsigned long long)s->zero, (unsigned long long)s->nan,
                    (unsigned long long)s->inf, (unsigned long long)s->n);
        } else {
            for (int c = 0; c < 14; c++) fprintf(out, "\t");
        }
        fprintf(out, "\t%s\n", field(note.c_str()).c_str());
        fflush(out);
    };

    auto measure_views = [&](const Queue& q, const PIX_EVENT_INFO& e, int pass, const std::string& call,
                             const std::string& programs, const std::vector<View>& views, bool outputs_only,
                             bool before) {
        size_t done = 0;
        for (auto& v : views) {
            bool output = v.type == PIX_RESOURCE_RENDER_TARGET_VIEW || v.type == PIX_RESOURCE_DEPTH_STENCIL_VIEW ||
                          v.type == PIX_RESOURCE_UNORDERED_ACCESS_VIEW;
            if (outputs_only && !output) continue;
            if (!outputs_only && v.type == PIX_RESOURCE_RENDER_TARGET_VIEW) continue;
            if (done++ >= max_views) {
                row(q, e, pass, call, programs, before ? "before" : "after", v, nullptr, nullptr, "over --max-views");
                continue;
            }
            if (!v.texture) {
                row(q, e, pass, call, programs, before ? "before" : "after", v, nullptr, nullptr, "buffer");
                continue;
            }
            Texels t;
            HRESULT hr = S_OK;
            if (!read_view(analysis.p, v, e, before, t, hr)) {
                char note[64];
                snprintf(note, sizeof note, "read failed 0x%08lx", (unsigned long)hr);
                row(q, e, pass, call, programs, before ? "before" : "after", v, nullptr, nullptr, note);
                continue;
            }
            Stats s;
            measure(t, s);
            row(q, e, pass, call, programs, before ? "before" : "after", v, &t, &s,
                s.channels ? "" : "format not decoded");
        }
    };

    for (auto& q : qs) {
        if (queue_id >= 0 && (int)q.id != queue_id) continue;
        logf("queue %u %s: %u events", q.id, q.name.c_str(), q.count);
        std::string marker;
        int pass = 0;
        std::string pass_key;
        // The open pass: its first and last events, how many draws and
        // dispatches, its programs, and the last event's views and label.
        PIX_EVENT_INFO last{};
        bool open = false;
        UINT32 first_index = 0, works = 0;
        std::vector<std::string> pass_programs;
        std::vector<View> last_views;
        std::string last_call;
        auto close_pass = [&]() {
            if (open && !each) {
                std::string programs;
                for (size_t k = 0; k < pass_programs.size(); k++) programs += (k ? "," : "") + pass_programs[k];
                if (pass_programs.size() >= 24) programs += ",...";
                char span[96];
                snprintf(span, sizeof span, "events %u-%u, %u draws/dispatches; last: ", first_index, last.Index, works);
                measure_views(q, last, pass, span + last_call, programs, last_views, true, false);
                if (pass % 25 == 0) logf("queue %u: pass %d measured (event %u)", q.id, pass, last.Index);
            }
            open = false;
        };
        for (UINT32 i = 0; i < q.count; i++) {
            PIX_EVENT_INFO e{};
            if (FAILED(q.info->GetEvent(i, &e))) continue;
            std::string name = e.Name ? e.Name : "";
            std::string call = e.ApiCallData ? e.ApiCallData : "";
            if (xtp_marker(name)) { marker = name; continue; }
            if (xtp_marker(call)) { marker = call; continue; }
            if (call.find("SetMarker") != std::string::npos || name.find("SetMarker") != std::string::npos) {
                size_t at = call.find('"');
                std::string text = at == std::string::npos ? call : call.substr(at + 1);
                if (xtp_marker(text)) marker = text;
                continue;
            }
            if (!is_work(name)) continue;
            if (e.Index < from || e.Index > to) {
                marker.clear();
                continue;
            }
            std::vector<View> views;
            Com<IPixPipelineState> pso;
            std::string pso_name;
            collect_views(doc.p, analysis.p, accessed, e, views, &pso, &pso_name);
            std::string programs = marker.empty() ? pso_name : marker_programs(marker);
            if (programs.empty()) programs = pso_name;
            // A pass is known by its outputs; work whose outputs are not
            // known (a compute program's UAVs without --accessed) by its
            // programs.
            std::string key;
            for (auto& v : views)
                if (v.type == PIX_RESOURCE_RENDER_TARGET_VIEW || v.type == PIX_RESOURCE_DEPTH_STENCIL_VIEW ||
                    v.type == PIX_RESOURCE_UNORDERED_ACCESS_VIEW)
                    key += "|" + std::to_string((unsigned long long)v.id) + ":" + std::to_string(v.subresource);
            if (key.empty()) key = "programs " + programs;
            std::string label = trim(call.empty() ? name : call, 160) + " {" + trim(marker, 600) + "}";
            if (!pso_name.empty()) label += " [" + pso_name + "; " + depth_state(pso.p) + "]";
            if (key != pass_key) {
                close_pass();
                pass++;
                pass_key = key;
                first_index = e.Index;
                works = 0;
                pass_programs.clear();
            }
            works++;
            if (pass_programs.size() < 24 &&
                std::find(pass_programs.begin(), pass_programs.end(), programs) == pass_programs.end())
                pass_programs.push_back(programs);
            if (inputs) measure_views(q, e, pass, label, programs, views, false, true);
            if (depth_diff && name.rfind("Draw", 0) == 0) {
                for (auto& v : views) {
                    if (v.type != PIX_RESOURCE_DEPTH_STENCIL_VIEW) continue;
                    Texels a, b;
                    HRESULT hr = S_OK;
                    if (read_view(analysis.p, v, e, TRUE, a, hr) && read_view(analysis.p, v, e, FALSE, b, hr)) {
                        uint64_t n = changed_texels(a, b);
                        Stats s;
                        measure(b, s);
                        row(q, e, pass, label, programs, "depth-diff", v, &b, &s,
                            "depth texels changed: " + std::to_string((unsigned long long)n));
                    } else {
                        char note[64];
                        snprintf(note, sizeof note, "depth read failed 0x%08lx", (unsigned long)hr);
                        row(q, e, pass, label, programs, "depth-diff", v, nullptr, nullptr, note);
                    }
                }
            }
            if (each) measure_views(q, e, pass, label, programs, views, true, false);
            last = e;
            last_views = std::move(views);
            last_call = label;
            open = true;
            marker.clear();
        }
        close_pass();
    }
    fclose(out);
    analysis->StopAnalysis();
    analysis->Disconnect();
    logf("walk done");
    return 0;
}
