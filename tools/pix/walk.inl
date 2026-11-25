// xtp-pix walk: replays a capture and measures what its passes write (and,
// on request, read). Included at the end of xtp-pix.cpp.
//
//   xtp-pix walk <file.wpix> <out.tsv> [--queue ID] [--from N] [--to N]
//                [--each] [--inputs] [--accessed] [--depth-diff] [--max-views N]
//                [--program HASH]... [--passes] [--adapter TEXT] [--no-replay]
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
// --program (repeatable) keeps --each, --inputs and --depth-diff to the
// events whose XTP marker names that program hash, and skips the per-pass
// rows unless --passes is given too. --adapter replays on the first adapter
// whose name holds TEXT ("Basic Render" for WARP), else on the default one.
// --no-replay lists the passes, their views and their pipelines' depth
// state without replaying (PIX replays only with Windows Developer Mode
// on): no statistics then, and no UAVs reached through the heap.
//
// One row per measured view: the event, the pass, XTP's marker text, the
// view (kind, binding, resource name, size, format) and the statistics of
// the subresource the view selects: mean and max of the luminance
// (0.2126 R + 0.7152 G + 0.0722 B for three channels or more, else the
// first channel), mean and max of each channel, and the counts of texels
// that are zero in every channel, NaN or infinite.

#include "texels.h"

struct Texels {
    PIX_TEXTURE_INFO info{};
    PIX_TEXTURE_SUBRESOURCE_INFO sub{};
    std::vector<uint8_t> bytes;
    DXGI_FORMAT format = DXGI_FORMAT_UNKNOWN;
};

static void measure(const Texels& t, Stats& s) {
    const auto& fp = t.sub.PlacedSubresourceFootprint;
    measure_raw(t.format, t.bytes.data(), t.bytes.size(), fp.Footprint.Width, fp.Footprint.Height,
                fp.Footprint.Depth, fp.Footprint.RowPitch, (size_t)fp.Offset, s);
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

// What a buffer view holds at an event: its size, how many of its 32-bit
// words are zero, NaN or infinite as floats, the mean and max of the
// finite ones, and its first words. Views reaching past 64 MiB of their
// buffer are not read (XTP mirrors title memory in buffers of gigabytes).
static std::string buffer_note(IPixGpuCaptureAnalysis* a, const View& v, const PIX_EVENT_INFO& e, bool before) {
    UINT64 offset = 0, size = 0;
    Com<IPixBufferResourceView> bv;
    Com<IPixConstantBufferView> cv;
    if (SUCCEEDED(v.view->QueryInterface(__uuidof(IPixBufferResourceView), bv.put_void())) && bv) {
        offset = bv->GetBufferLocationOffset();
        size = bv->GetSizeInBytes();
    } else if (SUCCEEDED(v.view->QueryInterface(__uuidof(IPixConstantBufferView), cv.put_void())) && cv) {
        const D3D12_CONSTANT_BUFFER_VIEW_DESC* d = nullptr;
        offset = cv->GetBufferLocationOffset();
        if (SUCCEEDED(cv->GetDesc(&d)) && d) size = d->SizeInBytes;
    } else {
        return "buffer (no size)";
    }
    char buf[512];
    if (offset + size > (64ull << 20) || size == 0) {
        snprintf(buf, sizeof buf, "buffer at +%#llx, %llu bytes: not read", (unsigned long long)offset,
                 (unsigned long long)size);
        return buf;
    }
    PIX_GET_BUFFER_BYTES_PARAMETERS p{before, FALSE};
    Com<IPixBlob> blob;
    HRESULT hr = a->GetBufferBytesFromResourceView(v.view.p, &e, &p, __uuidof(IPixBlob), blob.put_void());
    if (FAILED(hr) || !blob) {
        snprintf(buf, sizeof buf, "buffer at +%#llx, %llu bytes: read failed 0x%08lx", (unsigned long long)offset,
                 (unsigned long long)size, (unsigned long)hr);
        return buf;
    }
    std::vector<uint8_t> bytes((size_t)blob->GetSizeBytes());
    blob->GetData(bytes.size(), bytes.data());
    // The blob covers the view's window; its own bytes are the last `size`.
    size_t start = bytes.size() >= size ? bytes.size() - (size_t)size : 0;
    uint64_t words = 0, zero = 0, nan = 0, inf = 0, finite = 0;
    double sum = 0, mx = -INFINITY;
    std::string head;
    for (size_t at = start; at + 4 <= bytes.size(); at += 4) {
        uint32_t u;
        float f;
        memcpy(&u, bytes.data() + at, 4);
        memcpy(&f, &u, 4);
        words++;
        if (!u) zero++;
        if (std::isnan(f)) nan++;
        else if (std::isinf(f)) inf++;
        else {
            finite++;
            sum += f;
            mx = std::max(mx, (double)f);
        }
        if (words <= 8) {
            char w[48];
            snprintf(w, sizeof w, "%s%08x(%g)", words > 1 ? " " : "", u, f);
            head += w;
        }
    }
    snprintf(buf, sizeof buf, "buffer at +%#llx, %llu bytes: %llu words, %llu zero, %llu NaN, %llu inf; float mean %g max %g; first: %s",
             (unsigned long long)offset, (unsigned long long)size, (unsigned long long)words, (unsigned long long)zero,
             (unsigned long long)nan, (unsigned long long)inf, finite ? sum / finite : 0.0, finite ? mx : 0.0,
             head.c_str());
    return buf;
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

static std::string unescape(const std::string& s) {
    std::string out;
    for (size_t i = 0; i < s.size(); i++) {
        if (s[i] == '&') {
            size_t semi = s.find(';', i);
            if (semi != std::string::npos && semi - i < 8) {
                std::string ent = s.substr(i + 1, semi - i - 1);
                char c = 0;
                if (ent == "lt" || ent == "#60") c = '<';
                else if (ent == "gt" || ent == "#62") c = '>';
                else if (ent == "amp" || ent == "#38") c = '&';
                else if (ent == "quot" || ent == "#34") c = '"';
                else if (ent == "apos" || ent == "#39") c = '\'';
                if (c) {
                    out += c;
                    i = semi;
                    continue;
                }
            }
        }
        out += s[i];
    }
    return out;
}

// The leaf elements of PIX's XML call data: (tag, text) in order.
static std::vector<std::pair<std::string, std::string>> leaves(const std::string& xml) {
    std::vector<std::pair<std::string, std::string>> out;
    size_t at = 0;
    while ((at = xml.find('<', at)) != std::string::npos) {
        size_t close = xml.find('>', at);
        if (close == std::string::npos) break;
        if (xml[at + 1] == '/') {
            at = close + 1;
            continue;
        }
        std::string tag = xml.substr(at + 1, close - at - 1);
        size_t next = xml.find('<', close + 1);
        if (next == std::string::npos) break;
        std::string end = "</" + tag + ">";
        if (xml.compare(next, end.size(), end) == 0) out.emplace_back(tag, unescape(xml.substr(close + 1, next - close - 1)));
        at = close + 1;
    }
    return out;
}

// "Name(Tag=value, ...)" from a call's XML, without its `this`.
static std::string compact_call(const std::string& name, const std::string& xml) {
    std::string out = name + "(";
    bool first = true;
    for (auto& [tag, text] : leaves(xml)) {
        if (tag == "this" || tag == "returnValue") continue;
        out += (first ? "" : ", ") + tag + "=" + text;
        first = false;
    }
    return out + ")";
}

static int walk(int argc, wchar_t** argv) {
    if (argc < 2) {
        logf("walk <file.wpix> <out.tsv> [--queue ID] [--from N] [--to N] [--each] [--inputs] [--accessed] [--depth-diff]");
        return 2;
    }
    std::wstring file = argv[0], outp = argv[1];
    int queue_id = -1;
    UINT32 from = 0, to = UINT32_MAX;
    bool each = false, inputs = false, accessed = false, depth_diff = false, passes = false;
    std::vector<std::string> only;
    std::string adapter_text;
    int flags = -1;
    bool no_replay = false;
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
        else if (a == L"--program") only.push_back(narrow(next().c_str()));
        else if (a == L"--passes") passes = true;
        else if (a == L"--adapter") adapter_text = narrow(next().c_str());
        else if (a == L"--flags") flags = _wtoi(next().c_str());
        else if (a == L"--no-replay") no_replay = true;
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
    if (no_replay) {
        accessed = false;
        depth_diff = false;
    } else {
    CHECK(doc->GetAnalysis(__uuidof(IPixGpuCaptureAnalysis), analysis.put_void()));
    PIX_CONNECTION_DESC_LOCAL local{PIX_CONNECTION_FLAG_LOCAL_NONE};
    PIX_CONNECTION_DESC cdesc{};
    cdesc.Type = PIX_CONNECTION_TYPE_LOCAL;
    cdesc.pLocal = &local;
    CHECK(analysis->Connect(&cdesc, nullptr));
    PIX_ANALYSIS_PARAMS params{PIX_ADAPTER_ID_DEFAULT, PIX_ANALYSIS_FLAG_NONE};
    {
        Com<IPixAdapters> adapters;
        if (SUCCEEDED(analysis->GetAdapters(__uuidof(IPixAdapters), adapters.put_void())) && adapters) {
            for (UINT64 k = 0; k < adapters->GetCount(); k++) {
                PIX_ADAPTER ad{};
                if (FAILED(adapters->GetAdapter(k, &ad))) continue;
                std::string n = narrow(ad.Name);
                bool pick = !adapter_text.empty() && n.find(adapter_text) != std::string::npos &&
                            params.Adapter == PIX_ADAPTER_ID_DEFAULT;
                if (pick) params.Adapter = ad.Id;
                logf("adapter %llu: %s%s", (unsigned long long)ad.Id, n.c_str(), pick ? " (replay here)" : "");
            }
        }
        if (!adapter_text.empty() && params.Adapter == PIX_ADAPTER_ID_DEFAULT) {
            logf("no adapter named like %s", adapter_text.c_str());
            return 1;
        }
        if (!adapter_text.empty()) params.Flags = PIX_ANALYSIS_FLAG_NONE | PIX_ANALYSIS_FLAG_IGNORE_INCOMPATIBILITIES;
        if (flags >= 0) params.Flags = (PIX_ANALYSIS_FLAGS)flags;
    }
    auto* anotes = new AnalysisNotes();
    logf("starting the analysis (replay)");
    CHECK(analysis->StartAnalysis(&params, anotes, nullptr));
    if (accessed) {
        logf("gathering the accessed resources (instrumented replay)");
        CHECK(analysis->GatherAccessedResources());
    }
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
            if (!analysis) {
                row(q, e, pass, call, programs, "bound", v, nullptr, nullptr, v.texture ? "texture" : "buffer");
                continue;
            }
            if (!v.texture) {
                row(q, e, pass, call, programs, before ? "before" : "after", v, nullptr, nullptr,
                    buffer_note(analysis.p, v, e, before));
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
            if (open && !each && (only.empty() || passes)) {
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
            // PIX names a SetMarker of metadata 0 (XTP's, UTF-16)
            // "<deprecated - use pix3.h instead> <text>".
            if (name.rfind("<deprecated", 0) == 0 && name.find("> ") != std::string::npos)
                name = name.substr(name.find("> ") + 2);
            if (xtp_marker(name)) { marker = name; continue; }
            if (name.find("SetMarker") != std::string::npos || name.find("Marker") != std::string::npos) {
                for (auto& [tag, text] : leaves(call))
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
            std::string label = trim(compact_call(name, call), 300) + " {" + trim(marker, 600) + "}";
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
            bool chosen = only.empty();
            for (auto& h : only)
                if (programs.find(h) != std::string::npos) chosen = true;
            if (inputs && chosen) measure_views(q, e, pass, label, programs, views, false, true);
            if (depth_diff && chosen && name.rfind("Draw", 0) == 0) {
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
            if (each && chosen) measure_views(q, e, pass, label, programs, views, true, false);
            last = e;
            last_views = std::move(views);
            last_call = label;
            open = true;
            marker.clear();
        }
        close_pass();
    }
    fclose(out);
    if (analysis) {
        analysis->StopAnalysis();
        analysis->Disconnect();
    }
    logf("walk done");
    return 0;
}

// xtp-pix pso: the shaders of chosen pipeline state objects, from the
// capture's normalized pipeline state streams.
static int pso(int argc, wchar_t** argv) {
    if (argc < 3) {
        logf("pso <file.wpix> <out dir> <object id>...");
        return 2;
    }
    std::set<unsigned long long> wanted;
    for (int i = 2; i < argc; i++) wanted.insert(_wcstoui64(argv[i], nullptr, 10));
    Com<IPixFactory> factory;
    Com<IPixGpuCaptureDocument> doc;
    if (open_capture(argv[0], factory, doc)) return 1;
    fs::path out = argv[1];
    std::error_code ec;
    fs::create_directories(out, ec);
    // Pipeline state objects are not in the API object collection: they are
    // reached through the program state of a draw or dispatch that uses them.
    std::vector<Queue> qs;
    if (queues(doc.p, qs)) return 1;
    int found = 0;
    std::set<unsigned long long> done;
    for (auto& q : qs)
    for (UINT32 ev = 0; ev < q.count && done.size() < wanted.size(); ev++) {
        PIX_EVENT_INFO e{};
        if (FAILED(q.info->GetEvent(ev, &e)) || !e.Name || !is_work(e.Name)) continue;
        Com<IPixProgramState> ps;
        if (FAILED(doc->GetProgramState(&e, __uuidof(IPixProgramState), ps.put_void())) || !ps) continue;
        Com<IPixGenericPipeline> gp;
        if (FAILED(ps->GetGpuProgram(__uuidof(IPixGenericPipeline), gp.put_void())) || !gp) continue;
        Com<IPixPipelineState> p;
        if (FAILED(gp->GetPipelineState(__uuidof(IPixPipelineState), p.put_void())) || !p) continue;
        unsigned long long id = (unsigned long long)p->GetApiObjectId();
        if (!wanted.count(id) || !done.insert(id).second) continue;
        logf("pso %llu first used at event %u (%s)", id, e.Index, e.Name);
        const PIX_PIPELINE_STATE_STREAM_DESC* d = p->GetDesc();
        if (!d || !d->SubobjectStream) continue;
        const uint8_t* start = static_cast<const uint8_t*>(d->SubobjectStream);
        const uint8_t* at = start;
        const uint8_t* end = start + d->SizeInBytes;
        auto align = [](size_t x, size_t a) { return (x + a - 1) / a * a; };
        while (at + 4 <= end) {
            auto type = *reinterpret_cast<const D3D12_PIPELINE_STATE_SUBOBJECT_TYPE*>(at);
            const char* stage = nullptr;
            switch (type) {
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS: stage = "vs"; break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS: stage = "ps"; break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS: stage = "gs"; break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS: stage = "cs"; break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS: stage = "ms"; break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS: stage = "as"; break;
            default: break;
            }
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
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER: inner = sizeof(D3D12_RASTERIZER_DESC); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1: inner = sizeof(D3D12_RASTERIZER_DESC1); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2: inner = sizeof(D3D12_RASTERIZER_DESC2); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL: inner = sizeof(D3D12_DEPTH_STENCIL_DESC); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1: inner = sizeof(D3D12_DEPTH_STENCIL_DESC1); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2: inner = sizeof(D3D12_DEPTH_STENCIL_DESC2); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT: inner_align = 8; inner = sizeof(D3D12_INPUT_LAYOUT_DESC); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS: inner = sizeof(D3D12_RT_FORMAT_ARRAY); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC: inner = sizeof(DXGI_SAMPLE_DESC); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO: inner_align = 8; inner = sizeof(D3D12_CACHED_PIPELINE_STATE); break;
            case D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING: inner_align = 8; inner = sizeof(D3D12_VIEW_INSTANCING_DESC); break;
            default: inner = 4; break;
            }
            const uint8_t* body = at + align(4, inner_align);
            if (body + inner > end) break;
            if (stage) {
                auto* bc = reinterpret_cast<const D3D12_SHADER_BYTECODE*>(body);
                if (bc->pShaderBytecode && bc->BytecodeLength) {
                    fs::path f = out / (std::to_wstring(id) + L"-" + widen(stage) + L".dxil");
                    FILE* o = nullptr;
                    if (!_wfopen_s(&o, f.c_str(), L"wb") && o) {
                        fwrite(bc->pShaderBytecode, 1, bc->BytecodeLength, o);
                        fclose(o);
                        logf("pso %llu: %s, %zu bytes -> %s", id, stage, (size_t)bc->BytecodeLength, narrow(f.c_str()).c_str());
                        found++;
                    }
                }
            }
            at = start + align((size_t)(body + inner - start), sizeof(void*));
        }
    }
    for (auto id : wanted)
        if (!done.count(id)) logf("pso %llu: no direct draw or dispatch uses it", id);
    logf("%d shaders written", found);
    return found ? 0 : 1;
}
