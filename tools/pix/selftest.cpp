// A known frame for xtp-pix to capture and walk, on WARP only (it never
// touches a hardware GPU), so that the tool's capture, replay, readback and
// statistics can be checked against values set here:
//
//   draw     a triangle over all of a 256x128 RGBA16F target A, colour
//            (0.5, 0.25, 0.125, 1): mean and max luminance 0.294125; it
//            writes depth 0.25 into a 256x128 D32 target cleared to 1:
//            32,768 depth texels change;
//   dispatch reads A as a texture and writes 4 * A.r = 2.0 into a 64x64
//            R32F UAV C: mean and max 2.0; A, read before it: 0.294125.
//
//   selftest.exe <trigger dir>
//       Renders and presents (hidden window) for 60 s; after 20 frames it
//       writes <trigger dir>\frame-3.png (empty), the file `xtp-pix
//       capture --trigger-dir` waits for.
// tools/pix/build.ps1 builds it beside xtp-pix.exe; tools/pix/selftest.ps1
// runs the whole check.

#define NOMINMAX
#include <windows.h>
#include <d3d12.h>
#include <d3dcompiler.h>
#include <dxgi1_6.h>

#include <cstdio>
#include <string>

#define OK(x)                                                                          \
    do {                                                                               \
        HRESULT hr_ = (x);                                                             \
        if (FAILED(hr_)) {                                                             \
            fprintf(stderr, "%s failed: 0x%08lx (line %d)\n", #x, (unsigned long)hr_, __LINE__); \
            ExitProcess(1);                                                            \
        }                                                                              \
    } while (0)

template <class T>
static void release(T*& p) {
    if (p) p->Release();
    p = nullptr;
}

static ID3DBlob* compile(const char* src, const char* entry, const char* target) {
    ID3DBlob *code = nullptr, *errors = nullptr;
    HRESULT hr = D3DCompile(src, strlen(src), nullptr, nullptr, nullptr, entry, target, 0, 0, &code, &errors);
    if (FAILED(hr)) {
        fprintf(stderr, "%s: %s\n", target, errors ? (const char*)errors->GetBufferPointer() : "?");
        ExitProcess(1);
    }
    release(errors);
    return code;
}

static const char* kShaders = R"(
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0.25, 1);
}
float4 ps() : SV_Target { return float4(0.5, 0.25, 0.125, 1); }
Texture2D<float4> A : register(t0);
RWTexture2D<float> C : register(u0);
[numthreads(8, 8, 1)] void cs(uint3 id : SV_DispatchThreadID) { C[id.xy] = A[id.xy].r * 4; }
)";

static D3D12_RESOURCE_BARRIER transition(ID3D12Resource* r, D3D12_RESOURCE_STATES a, D3D12_RESOURCE_STATES b) {
    D3D12_RESOURCE_BARRIER x{};
    x.Type = D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
    x.Transition.pResource = r;
    x.Transition.Subresource = D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
    x.Transition.StateBefore = a;
    x.Transition.StateAfter = b;
    return x;
}

static ID3D12Resource* texture(ID3D12Device* dev, UINT w, UINT h, DXGI_FORMAT f, D3D12_RESOURCE_FLAGS flags,
                               D3D12_RESOURCE_STATES state, const D3D12_CLEAR_VALUE* clear, const wchar_t* name) {
    D3D12_HEAP_PROPERTIES hp{D3D12_HEAP_TYPE_DEFAULT};
    D3D12_RESOURCE_DESC d{};
    d.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    d.Width = w;
    d.Height = h;
    d.DepthOrArraySize = 1;
    d.MipLevels = 1;
    d.Format = f;
    d.SampleDesc.Count = 1;
    d.Flags = flags;
    ID3D12Resource* r = nullptr;
    OK(dev->CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &d, state, clear, IID_PPV_ARGS(&r)));
    r->SetName(name);
    return r;
}

int wmain(int argc, wchar_t** argv) {
    if (argc < 2) {
        fprintf(stderr, "selftest.exe <trigger dir>\n");
        return 2;
    }
    std::wstring trigger = std::wstring(argv[1]) + L"\\frame-3.png";

    IDXGIFactory4* factory = nullptr;
    OK(CreateDXGIFactory2(0, IID_PPV_ARGS(&factory)));
    IDXGIAdapter* warp = nullptr;
    OK(factory->EnumWarpAdapter(IID_PPV_ARGS(&warp)));
    ID3D12Device* dev = nullptr;
    OK(D3D12CreateDevice(warp, D3D_FEATURE_LEVEL_11_0, IID_PPV_ARGS(&dev)));

    D3D12_COMMAND_QUEUE_DESC qd{D3D12_COMMAND_LIST_TYPE_DIRECT};
    ID3D12CommandQueue* queue = nullptr;
    OK(dev->CreateCommandQueue(&qd, IID_PPV_ARGS(&queue)));

    WNDCLASSW wc{};
    wc.lpfnWndProc = DefWindowProcW;
    wc.hInstance = GetModuleHandleW(nullptr);
    wc.lpszClassName = L"xtp-pix-selftest";
    RegisterClassW(&wc);
    HWND hwnd = CreateWindowW(wc.lpszClassName, L"xtp-pix selftest", WS_OVERLAPPEDWINDOW, 0, 0, 256, 128, nullptr,
                              nullptr, wc.hInstance, nullptr);  // never shown
    DXGI_SWAP_CHAIN_DESC1 sd{};
    sd.Width = 256;
    sd.Height = 128;
    sd.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    sd.SampleDesc.Count = 1;
    sd.BufferUsage = DXGI_USAGE_RENDER_TARGET_OUTPUT;
    sd.BufferCount = 2;
    sd.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    IDXGISwapChain1* sc1 = nullptr;
    OK(factory->CreateSwapChainForHwnd(queue, hwnd, &sd, nullptr, nullptr, &sc1));
    IDXGISwapChain3* sc = nullptr;
    OK(sc1->QueryInterface(IID_PPV_ARGS(&sc)));

    D3D12_CLEAR_VALUE cv{DXGI_FORMAT_R16G16B16A16_FLOAT, {0, 0, 0, 0}};
    ID3D12Resource* a = texture(dev, 256, 128, DXGI_FORMAT_R16G16B16A16_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                                D3D12_RESOURCE_STATE_RENDER_TARGET, &cv, L"selftest A");
    D3D12_CLEAR_VALUE dv{DXGI_FORMAT_D32_FLOAT};
    dv.DepthStencil.Depth = 1;
    ID3D12Resource* depth = texture(dev, 256, 128, DXGI_FORMAT_D32_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                                    D3D12_RESOURCE_STATE_DEPTH_WRITE, &dv, L"selftest depth");
    ID3D12Resource* c = texture(dev, 64, 64, DXGI_FORMAT_R32_FLOAT, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS, nullptr, L"selftest C");

    D3D12_DESCRIPTOR_HEAP_DESC rh{D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 3};
    ID3D12DescriptorHeap* rtvs = nullptr;
    OK(dev->CreateDescriptorHeap(&rh, IID_PPV_ARGS(&rtvs)));
    D3D12_DESCRIPTOR_HEAP_DESC dh{D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 1};
    ID3D12DescriptorHeap* dsvs = nullptr;
    OK(dev->CreateDescriptorHeap(&dh, IID_PPV_ARGS(&dsvs)));
    D3D12_DESCRIPTOR_HEAP_DESC vh{D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 2, D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE};
    ID3D12DescriptorHeap* views = nullptr;
    OK(dev->CreateDescriptorHeap(&vh, IID_PPV_ARGS(&views)));
    UINT rtv_step = dev->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
    UINT view_step = dev->GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
    auto rtv = [&](UINT i) {
        D3D12_CPU_DESCRIPTOR_HANDLE h = rtvs->GetCPUDescriptorHandleForHeapStart();
        h.ptr += (SIZE_T)i * rtv_step;
        return h;
    };
    dev->CreateRenderTargetView(a, nullptr, rtv(0));
    ID3D12Resource* back[2] = {};
    for (UINT i = 0; i < 2; i++) {
        OK(sc->GetBuffer(i, IID_PPV_ARGS(&back[i])));
        dev->CreateRenderTargetView(back[i], nullptr, rtv(1 + i));
    }
    dev->CreateDepthStencilView(depth, nullptr, dsvs->GetCPUDescriptorHandleForHeapStart());
    D3D12_CPU_DESCRIPTOR_HANDLE vcpu = views->GetCPUDescriptorHandleForHeapStart();
    dev->CreateShaderResourceView(a, nullptr, vcpu);
    vcpu.ptr += view_step;
    dev->CreateUnorderedAccessView(c, nullptr, nullptr, vcpu);

    // Root signatures: none for the draw, one table (t0, u0) for the dispatch.
    D3D12_DESCRIPTOR_RANGE ranges[2] = {{D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 1, 0, 0, 0}, {D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 1, 0, 0, 1}};
    D3D12_ROOT_PARAMETER param{};
    param.ParameterType = D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE;
    param.DescriptorTable = {2, ranges};
    param.ShaderVisibility = D3D12_SHADER_VISIBILITY_ALL;
    ID3D12RootSignature *rs_draw = nullptr, *rs_cs = nullptr;
    for (int k = 0; k < 2; k++) {
        D3D12_ROOT_SIGNATURE_DESC rd{};
        rd.NumParameters = k;
        rd.pParameters = k ? &param : nullptr;
        ID3DBlob *blob = nullptr, *err = nullptr;
        OK(D3D12SerializeRootSignature(&rd, D3D_ROOT_SIGNATURE_VERSION_1, &blob, &err));
        OK(dev->CreateRootSignature(0, blob->GetBufferPointer(), blob->GetBufferSize(),
                                    IID_PPV_ARGS(k ? &rs_cs : &rs_draw)));
        release(blob);
        release(err);
    }
    ID3DBlob* vs = compile(kShaders, "vs", "vs_5_0");
    ID3DBlob* ps = compile(kShaders, "ps", "ps_5_0");
    ID3DBlob* cs = compile(kShaders, "cs", "cs_5_0");
    D3D12_GRAPHICS_PIPELINE_STATE_DESC gd{};
    gd.pRootSignature = rs_draw;
    gd.VS = {vs->GetBufferPointer(), vs->GetBufferSize()};
    gd.PS = {ps->GetBufferPointer(), ps->GetBufferSize()};
    gd.BlendState.RenderTarget[0].RenderTargetWriteMask = D3D12_COLOR_WRITE_ENABLE_ALL;
    gd.SampleMask = UINT_MAX;
    gd.RasterizerState.FillMode = D3D12_FILL_MODE_SOLID;
    gd.RasterizerState.CullMode = D3D12_CULL_MODE_NONE;
    gd.RasterizerState.DepthClipEnable = TRUE;
    gd.DepthStencilState.DepthEnable = TRUE;
    gd.DepthStencilState.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ALL;
    gd.DepthStencilState.DepthFunc = D3D12_COMPARISON_FUNC_LESS;
    gd.PrimitiveTopologyType = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
    gd.NumRenderTargets = 1;
    gd.RTVFormats[0] = DXGI_FORMAT_R16G16B16A16_FLOAT;
    gd.DSVFormat = DXGI_FORMAT_D32_FLOAT;
    gd.SampleDesc.Count = 1;
    ID3D12PipelineState *draw = nullptr, *dispatch = nullptr;
    OK(dev->CreateGraphicsPipelineState(&gd, IID_PPV_ARGS(&draw)));
    D3D12_COMPUTE_PIPELINE_STATE_DESC cd{};
    cd.pRootSignature = rs_cs;
    cd.CS = {cs->GetBufferPointer(), cs->GetBufferSize()};
    OK(dev->CreateComputePipelineState(&cd, IID_PPV_ARGS(&dispatch)));

    ID3D12CommandAllocator* alloc = nullptr;
    OK(dev->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&alloc)));
    ID3D12GraphicsCommandList* list = nullptr;
    OK(dev->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc, nullptr, IID_PPV_ARGS(&list)));
    OK(list->Close());
    ID3D12Fence* fence = nullptr;
    OK(dev->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence)));
    HANDLE done = CreateEventW(nullptr, FALSE, FALSE, nullptr);
    UINT64 value = 0;

    DWORD start = GetTickCount();
    for (UINT frame = 0; GetTickCount() - start < 60000; frame++) {
        OK(alloc->Reset());
        OK(list->Reset(alloc, nullptr));
        const float zero[4] = {0, 0, 0, 0};
        list->ClearRenderTargetView(rtv(0), zero, 0, nullptr);
        list->ClearDepthStencilView(dsvs->GetCPUDescriptorHandleForHeapStart(), D3D12_CLEAR_FLAG_DEPTH, 1, 0, 0, nullptr);
        D3D12_VIEWPORT vp{0, 0, 256, 128, 0, 1};
        D3D12_RECT sr{0, 0, 256, 128};
        list->RSSetViewports(1, &vp);
        list->RSSetScissorRects(1, &sr);
        D3D12_CPU_DESCRIPTOR_HANDLE target = rtv(0), dsv = dsvs->GetCPUDescriptorHandleForHeapStart();
        list->OMSetRenderTargets(1, &target, FALSE, &dsv);
        list->SetGraphicsRootSignature(rs_draw);
        list->SetPipelineState(draw);
        list->IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list->DrawInstanced(3, 1, 0, 0);
        D3D12_RESOURCE_BARRIER b = transition(a, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE);
        list->ResourceBarrier(1, &b);
        list->SetDescriptorHeaps(1, &views);
        list->SetComputeRootSignature(rs_cs);
        list->SetPipelineState(dispatch);
        list->SetComputeRootDescriptorTable(0, views->GetGPUDescriptorHandleForHeapStart());
        list->Dispatch(8, 8, 1);
        b = transition(a, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE, D3D12_RESOURCE_STATE_RENDER_TARGET);
        list->ResourceBarrier(1, &b);
        UINT i = sc->GetCurrentBackBufferIndex();
        b = transition(back[i], D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET);
        list->ResourceBarrier(1, &b);
        const float grey[4] = {0.5f, 0.5f, 0.5f, 1};
        list->ClearRenderTargetView(rtv(1 + i), grey, 0, nullptr);
        b = transition(back[i], D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATE_PRESENT);
        list->ResourceBarrier(1, &b);
        OK(list->Close());
        ID3D12CommandList* lists[] = {list};
        queue->ExecuteCommandLists(1, lists);
        sc->Present(0, 0);
        OK(queue->Signal(fence, ++value));
        OK(fence->SetEventOnCompletion(value, done));
        WaitForSingleObject(done, INFINITE);
        if (frame == 20) {
            HANDLE f = CreateFileW(trigger.c_str(), GENERIC_WRITE, 0, nullptr, CREATE_ALWAYS, 0, nullptr);
            if (f != INVALID_HANDLE_VALUE) CloseHandle(f);
        }
        Sleep(16);
    }
    return 0;
}
