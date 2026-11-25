// xtp-rdoc: the same frame walk as xtp-pix, through RenderDoc's replay API
// (renderdoc.dll of E:\tools\gpu\RenderDoc, 1.47), which replays without
// Windows Developer Mode. Built by tools/pix/build.ps1 against RenderDoc's
// API headers at the DLL's own commit, fetched outside the repository.
//
//   xtp-rdoc capture --exe <exe> --cwd <dir> --args "<command line>"
//                    [--env K=V]... --trigger-dir <dir> [--settle-s N]
//                    [--timeout-s N] [--soft-memory-mb N] --out <file.rdc>
//                    [--log <file>]
//       Starts the exe with RenderDoc injected, waits for an
//       XTP_CAPTURE_WORLD frame-<n>.png (n not a power of two) in the
//       trigger folder, waits N seconds, has RenderDoc capture one frame,
//       copies it to --out and ends the process.
//   xtp-rdoc walk <file.rdc> <out.tsv> [--from EID] [--to EID] [--each]
//                 [--inputs] [--program HASH]... [--passes] [--depth-diff]
//                 [--max-views N] [--software] [--writes ID]...
//       Replays the capture and writes, for each pass (a run of draws and
//       dispatches with the same outputs), the statistics of what it wrote:
//       render targets, depth, and the textures its shaders wrote (as
//       RenderDoc's descriptor access reports them, heap-indexed ones
//       included). --each measures after every draw and dispatch in range,
//       --inputs what each reads before it, --program limits those to the
//       events after an XTP marker naming that program hash (and skips the
//       per-pass rows unless --passes), --depth-diff counts the depth texels
//       each draw changed, --software replays on WARP, --writes keeps those
//       to the events writing that resource (ids as the table prints them;
//       with --program, either picks an event). The columns are
//       xtp-pix walk's.

#define NOMINMAX
#define RENDERDOC_PLATFORM_WIN32
#include <windows.h>

#include <algorithm>
#include <chrono>
#include <cstdarg>
#include <cstdio>
#include <filesystem>
#include <map>
#include <set>
#include <string>
#include <thread>
#include <vector>

#include "api/replay/renderdoc_replay.h"
#include "api/replay/pipestate.inl"
#include "api/replay/renderdoc_tostr.inl"
#include "texels.h"

// Tells renderdoc.dll that this program replays (it hooks nothing then).
REPLAY_PROGRAM_MARKER()

// The one stringiser renderdoc_tostr.inl's bitmask printers call that lives
// in RenderDoc's core rather than in its API headers.
template <>
rdcstr DoStringise(const uint32_t& el) {
    return std::to_string(el).c_str();
}

namespace fs = std::filesystem;

static FILE* g_log = nullptr;

static void logf(const char* fmt, ...) {
    char buf[4096];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    SYSTEMTIME t;
    GetLocalTime(&t);
    fprintf(stderr, "%02d:%02d:%02d %s\n", t.wHour, t.wMinute, t.wSecond, buf);
    if (g_log) {
        fprintf(g_log, "%02d:%02d:%02d %s\n", t.wHour, t.wMinute, t.wSecond, buf);
        fflush(g_log);
    }
}

static std::string narrow(const wchar_t* w) {
    if (!w) return {};
    int n = WideCharToMultiByte(CP_UTF8, 0, w, -1, nullptr, 0, nullptr, nullptr);
    std::string s(n > 0 ? n - 1 : 0, '\0');
    if (n > 1) WideCharToMultiByte(CP_UTF8, 0, w, -1, s.data(), n, nullptr, nullptr);
    return s;
}

static std::string field(const std::string& s) {
    std::string out = s;
    for (auto& c : out)
        if (c == '\t' || c == '\n' || c == '\r') c = ' ';
    return out;
}

static bool is_power_of_two(uint64_t n) { return n && !(n & (n - 1)); }

static bool world_frame_written(const fs::path& dir, std::string& name) {
    std::error_code ec;
    for (auto& e : fs::directory_iterator(dir, ec)) {
        std::string f = narrow(e.path().filename().c_str());
        unsigned long long n = 0;
        char tail[16] = {};
        if (sscanf_s(f.c_str(), "frame-%llu%15s", &n, tail, (unsigned)sizeof tail) == 2 && strcmp(tail, ".png") == 0 &&
            !is_power_of_two(n)) {
            name = f;
            return true;
        }
    }
    return false;
}

// ---------------------------------------------------------------------------
// capture

static int capture(int argc, wchar_t** argv) {
    std::string exe, cwd, args, out, log;
    fs::path trigger;
    rdcarray<EnvironmentModification> env;
    int settle = 5, timeout = 900, capture_timeout = 900, soft_mb = 0;
    for (int i = 0; i < argc; i++) {
        std::wstring a = argv[i];
        auto next = [&]() -> std::string { return i + 1 < argc ? narrow(argv[++i]) : ""; };
        if (a == L"--exe") exe = next();
        else if (a == L"--cwd") cwd = next();
        else if (a == L"--args") args = next();
        else if (a == L"--out") out = next();
        else if (a == L"--log") log = next();
        else if (a == L"--trigger-dir") trigger = fs::u8path(next());
        else if (a == L"--settle-s") settle = atoi(next().c_str());
        else if (a == L"--timeout-s") timeout = atoi(next().c_str());
        else if (a == L"--capture-timeout-s") capture_timeout = atoi(next().c_str());
        else if (a == L"--soft-memory-mb") soft_mb = atoi(next().c_str());
        else if (a == L"--env") {
            std::string kv = next();
            size_t eq = kv.find('=');
            if (eq == std::string::npos) {
                logf("--env %s: expected K=V", kv.c_str());
                return 2;
            }
            env.push_back(EnvironmentModification(EnvMod::Set, EnvSep::NoSep, kv.substr(0, eq).c_str(),
                                                  kv.substr(eq + 1).c_str()));
        } else {
            logf("capture: unknown argument %s", narrow(a.c_str()).c_str());
            return 2;
        }
    }
    if (exe.empty() || out.empty() || trigger.empty()) {
        logf("capture needs --exe, --out and --trigger-dir");
        return 2;
    }
    if (!log.empty()) fopen_s(&g_log, log.c_str(), "a");
    logf("RenderDoc %s", RENDERDOC_GetVersionString());
    CaptureOptions opts;
    opts.allowVSync = true;
    opts.allowFullscreen = false;
    opts.softMemoryLimit = (uint32_t)soft_mb;
    std::string folder = fs::u8path(out).parent_path().u8string();
    std::string tmpl = folder + "\\rdoc";
    ExecuteResult r = RENDERDOC_ExecuteAndInject(exe.c_str(), cwd.c_str(), args.c_str(), env, tmpl.c_str(), opts, false);
    if (r.result.code != ResultCode::Succeeded || !r.ident) {
        logf("ExecuteAndInject failed: %s", r.result.Message().c_str());
        return 1;
    }
    logf("launched with RenderDoc, target control ident %u", r.ident);
    ITargetControl* tc = RENDERDOC_CreateTargetControl("", r.ident, "xtp-rdoc", true);
    if (!tc || !tc->Connected()) {
        logf("no target control connection");
        return 1;
    }
    uint32_t pid = tc->GetPID();
    logf("target %s pid %u", tc->GetTarget().c_str(), pid);
    printf("pid %u\n", pid);
    fflush(stdout);
    HANDLE proc = OpenProcess(SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
    auto alive = [&]() { return proc && WaitForSingleObject(proc, 0) == WAIT_TIMEOUT; };
    auto end_target = [&]() {
        tc->Shutdown();
        if (alive()) TerminateProcess(proc, 1);
    };
    auto pump = [&](std::string* path) {
        TargetControlMessage m = tc->ReceiveMessage(nullptr);
        if (m.type == TargetControlMessageType::RegisterAPI) logf("target uses %s", m.apiUse.name.c_str());
        else if (m.type == TargetControlMessageType::Disconnected) logf("target control disconnected");
        else if (m.type == TargetControlMessageType::NewCapture) {
            logf("new capture %s (%s)", m.newCapture.path.c_str(), m.newCapture.local ? "local" : "remote");
            if (path) *path = m.newCapture.path.c_str();
        }
        return m.type;
    };
    auto start = std::chrono::steady_clock::now();
    std::string seen;
    while (!world_frame_written(trigger, seen)) {
        if (!alive() || pump(nullptr) == TargetControlMessageType::Disconnected) {
            logf("the target ended before the world frame appeared");
            end_target();
            return 3;
        }
        if (std::chrono::steady_clock::now() - start > std::chrono::seconds(timeout)) {
            logf("no world frame after %d s", timeout);
            end_target();
            return 4;
        }
        Sleep(500);
    }
    logf("world frame %s seen; settling %d s", seen.c_str(), settle);
    auto settled = std::chrono::steady_clock::now() + std::chrono::seconds(settle);
    while (std::chrono::steady_clock::now() < settled) {
        pump(nullptr);
        Sleep(200);
    }
    logf("triggering a one-frame capture");
    tc->TriggerCapture(1);
    std::string path;
    auto asked = std::chrono::steady_clock::now();
    while (path.empty()) {
        if (!alive()) {
            logf("the target ended before the capture was written");
            end_target();
            return 5;
        }
        if (pump(&path) == TargetControlMessageType::Disconnected) break;
        if (std::chrono::steady_clock::now() - asked > std::chrono::seconds(capture_timeout)) {
            logf("no capture after %d s", capture_timeout);
            break;
        }
    }
    end_target();
    if (path.empty()) return 5;
    std::error_code ec;
    fs::rename(fs::u8path(path), fs::u8path(out), ec);
    if (ec) {
        fs::copy_file(fs::u8path(path), fs::u8path(out), fs::copy_options::overwrite_existing, ec);
        if (ec) {
            logf("copying %s: %s", path.c_str(), ec.message().c_str());
            return 6;
        }
    }
    logf("saved %s (%llu bytes)", out.c_str(), (unsigned long long)fs::file_size(fs::u8path(out), ec));
    return 0;
}

// ---------------------------------------------------------------------------
// walk

// The DXGI format of RenderDoc's description of one, for texels.h; UNKNOWN
// for the ones it does not read.
static DXGI_FORMAT dxgi(const ResourceFormat& f) {
    switch (f.type) {
    case ResourceFormatType::R10G10B10A2:
        return f.compType == CompType::UInt ? DXGI_FORMAT_R10G10B10A2_UINT : DXGI_FORMAT_R10G10B10A2_UNORM;
    case ResourceFormatType::R11G11B10: return DXGI_FORMAT_R11G11B10_FLOAT;
    case ResourceFormatType::R9G9B9E5: return DXGI_FORMAT_R9G9B9E5_SHAREDEXP;
    case ResourceFormatType::D24S8: return DXGI_FORMAT_D24_UNORM_S8_UINT;
    case ResourceFormatType::D32S8: return DXGI_FORMAT_D32_FLOAT_S8X24_UINT;
    case ResourceFormatType::A8: return DXGI_FORMAT_A8_UNORM;
    case ResourceFormatType::Regular: break;
    default: return DXGI_FORMAT_UNKNOWN;
    }
    int n = f.compCount, w = f.compByteWidth;
    bool bgra = f.BGRAOrder();
    switch (f.compType) {
    case CompType::Float:
        if (w == 4) return n == 1 ? DXGI_FORMAT_R32_FLOAT : n == 2 ? DXGI_FORMAT_R32G32_FLOAT : n == 3 ? DXGI_FORMAT_R32G32B32_FLOAT : DXGI_FORMAT_R32G32B32A32_FLOAT;
        if (w == 2) return n == 1 ? DXGI_FORMAT_R16_FLOAT : n == 2 ? DXGI_FORMAT_R16G16_FLOAT : n == 4 ? DXGI_FORMAT_R16G16B16A16_FLOAT : DXGI_FORMAT_UNKNOWN;
        break;
    case CompType::Depth:
        if (w == 4 && n == 1) return DXGI_FORMAT_D32_FLOAT;
        if (w == 2 && n == 1) return DXGI_FORMAT_D16_UNORM;
        break;
    case CompType::UNorm: case CompType::Typeless:
        if (w == 1) return n == 1 ? DXGI_FORMAT_R8_UNORM : n == 2 ? DXGI_FORMAT_R8G8_UNORM : n == 4 ? (bgra ? DXGI_FORMAT_B8G8R8A8_UNORM : DXGI_FORMAT_R8G8B8A8_UNORM) : DXGI_FORMAT_UNKNOWN;
        if (w == 2) return n == 1 ? DXGI_FORMAT_R16_UNORM : n == 2 ? DXGI_FORMAT_R16G16_UNORM : n == 4 ? DXGI_FORMAT_R16G16B16A16_UNORM : DXGI_FORMAT_UNKNOWN;
        if (w == 4 && f.compType == CompType::Typeless) return n == 1 ? DXGI_FORMAT_R32_UINT : n == 2 ? DXGI_FORMAT_R32G32_UINT : n == 4 ? DXGI_FORMAT_R32G32B32A32_UINT : DXGI_FORMAT_UNKNOWN;
        break;
    case CompType::UNormSRGB:
        if (w == 1 && n == 4) return bgra ? DXGI_FORMAT_B8G8R8A8_UNORM_SRGB : DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
        break;
    case CompType::SNorm:
        if (w == 1 && n == 4) return DXGI_FORMAT_R8G8B8A8_SNORM;
        if (w == 2 && n == 2) return DXGI_FORMAT_R16G16_SNORM;
        break;
    case CompType::UInt: case CompType::SInt:
        if (w == 1) return n == 1 ? DXGI_FORMAT_R8_UINT : n == 2 ? DXGI_FORMAT_R8G8_UINT : n == 4 ? DXGI_FORMAT_R8G8B8A8_UINT : DXGI_FORMAT_UNKNOWN;
        if (w == 2) return n == 1 ? DXGI_FORMAT_R16_UINT : n == 2 ? DXGI_FORMAT_R16G16_UINT : n == 4 ? DXGI_FORMAT_R16G16B16A16_UINT : DXGI_FORMAT_UNKNOWN;
        if (w == 4) return n == 1 ? DXGI_FORMAT_R32_UINT : n == 2 ? DXGI_FORMAT_R32G32_UINT : n == 4 ? DXGI_FORMAT_R32G32B32A32_UINT : DXGI_FORMAT_UNKNOWN;
        break;
    default: break;
    }
    return DXGI_FORMAT_UNKNOWN;
}

struct Out {
    const char* kind;   // RTV, DSV, UAV, SRV
    ResourceId id;
    uint32_t mip = 0, slice = 0;
    ResourceFormat format;
    std::string binding;
};

static std::string stage_name(ShaderStage s) {
    switch (s) {
    case ShaderStage::Vertex: return "vs";
    case ShaderStage::Pixel: return "ps";
    case ShaderStage::Compute: return "cs";
    case ShaderStage::Mesh: return "ms";
    case ShaderStage::Amplification: return "as";
    default: return "stage";
    }
}

static std::string id_text(ResourceId id) {
    uint64_t raw = 0;
    static_assert(sizeof(ResourceId) == sizeof(uint64_t), "ResourceId is 64 bits");
    memcpy(&raw, &id, sizeof raw);
    return std::to_string((unsigned long long)raw);
}

static bool xtp_marker(const std::string& s) {
    return s.rfind("draw ", 0) == 0 || s.rfind("indexed ", 0) == 0 || s.rfind("dispatch ", 0) == 0 ||
           s.rfind("mesh ", 0) == 0;
}

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

static void flatten(const rdcarray<ActionDescription>& in, std::vector<const ActionDescription*>& out) {
    for (const auto& a : in) {
        out.push_back(&a);
        flatten(a.children, out);
    }
}

static int walk(int argc, wchar_t** argv) {
    if (argc < 2) {
        logf("walk <file.rdc> <out.tsv> [options]");
        return 2;
    }
    std::string file = narrow(argv[0]), outp = narrow(argv[1]);
    uint32_t from = 0, to = UINT32_MAX;
    bool each = false, inputs = false, passes = false, depth_diff = false, software = false;
    size_t max_views = 64;
    std::vector<std::string> only, writes;
    for (int i = 2; i < argc; i++) {
        std::wstring a = argv[i];
        auto next = [&]() -> std::string { return i + 1 < argc ? narrow(argv[++i]) : "0"; };
        if (a == L"--from") from = (uint32_t)strtoul(next().c_str(), nullptr, 10);
        else if (a == L"--to") to = (uint32_t)strtoul(next().c_str(), nullptr, 10);
        else if (a == L"--each") each = true;
        else if (a == L"--inputs") inputs = true;
        else if (a == L"--passes") passes = true;
        else if (a == L"--depth-diff") depth_diff = true;
        else if (a == L"--software") software = true;
        else if (a == L"--program") only.push_back(next());
        else if (a == L"--writes") writes.push_back(next());
        else if (a == L"--max-views") max_views = (size_t)atoi(next().c_str());
        else if (a == L"--log") fopen_s(&g_log, next().c_str(), "a");
        else {
            logf("walk: unknown argument %s", narrow(a.c_str()).c_str());
            return 2;
        }
    }
    GlobalEnvironment genv;
    genv.enumerateGPUs = false;
    RENDERDOC_InitialiseReplay(genv, {});
    ICaptureFile* cap = RENDERDOC_OpenCaptureFile();
    ResultDetails r = cap->OpenFile(file.c_str(), "", nullptr);
    if (r.code != ResultCode::Succeeded) {
        logf("OpenFile: %s", r.Message().c_str());
        return 1;
    }
    ReplayOptions ropts;
    if (software) ropts.forceGPUVendor = GPUVendor::Software;
    logf("opening the capture for replay (local support %d)", (int)cap->LocalReplaySupport());
    rdcpair<ResultDetails, IReplayController*> opened = cap->OpenCapture(ropts, nullptr);
    if (opened.first.code != ResultCode::Succeeded || !opened.second) {
        logf("OpenCapture: %s", opened.first.Message().c_str());
        return 1;
    }
    IReplayController* ctrl = opened.second;
    std::map<std::string, TextureDescription> textures;
    for (const auto& t : ctrl->GetTextures()) textures[id_text(t.resourceId)] = t;
    std::map<std::string, std::string> names;
    for (const auto& d : ctrl->GetResources()) names[id_text(d.resourceId)] = d.name.c_str();
    std::vector<const ActionDescription*> actions;
    flatten(ctrl->GetRootActions(), actions);
    logf("%zu actions, %zu textures", actions.size(), textures.size());

    FILE* out = nullptr;
    if (fopen_s(&out, outp.c_str(), "w") || !out) {
        logf("cannot write %s", outp.c_str());
        return 1;
    }
    fprintf(out,
            "queue\tevent\tpass\tcall\tprograms\twhen\tkind\tbinding\tresource\tid\tsize\tformat\tmean_lum\tmax_lum\t"
            "mean_r\tmean_g\tmean_b\tmean_a\tmax_r\tmax_g\tmax_b\tmax_a\tzero\tnan\tinf\ttexels\tnote\n");

    auto read = [&](const Out& o, std::vector<uint8_t>& bytes, TextureDescription& t, DXGI_FORMAT& f) {
        auto it = textures.find(id_text(o.id));
        if (it == textures.end()) return false;
        t = it->second;
        bytebuf b = ctrl->GetTextureData(o.id, Subresource(o.mip, o.slice, 0));
        bytes.assign(b.data(), b.data() + b.size());
        f = dxgi(o.format.compType == CompType::Typeless && o.format.type == ResourceFormatType::Regular ? t.format : o.format);
        if (f == DXGI_FORMAT_UNKNOWN) f = dxgi(t.format);
        return true;
    };
    auto row = [&](uint32_t eid, int pass, const std::string& call, const std::string& programs, const char* when,
                   const Out& o, const std::string& note, bool measure) {
        std::vector<uint8_t> bytes;
        TextureDescription t{};
        DXGI_FORMAT f = DXGI_FORMAT_UNKNOWN;
        std::string size, fmt, extra = note;
        Stats s;
        if (measure && read(o, bytes, t, f)) {
            uint32_t w = std::max(1u, t.width >> o.mip), h = std::max(1u, t.height >> o.mip);
            uint32_t d = t.dimension == 3 ? std::max(1u, t.depth >> o.mip) : 1;
            int channels = 0;
            int texel = texel_bytes(f, channels);
            char buf[64];
            snprintf(buf, sizeof buf, "%ux%ux%u", w, h, d);
            size = buf;
            fmt = std::to_string((int)f);
            if (texel) measure_raw(f, bytes.data(), bytes.size(), w, h, d, (size_t)w * texel, 0, s);
            else extra += (extra.empty() ? "" : "; ") + std::string("format not decoded: ") + t.format.Name().c_str();
        } else if (measure) {
            extra += (extra.empty() ? "" : "; ") + std::string("not a texture");
        }
        std::string id = id_text(o.id);
        fprintf(out, "0\t%u\t%d\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s", eid, pass, field(call).c_str(),
                field(programs).c_str(), when, o.kind, field(o.binding).c_str(), field(names[id]).c_str(), id.c_str(),
                size.c_str(), fmt.c_str());
        if (s.n && s.channels) {
            uint64_t good = s.n - s.nan - s.inf;
            double g = good ? (double)good : 1.0;
            fprintf(out, "\t%.6g\t%.6g", s.sum_lum / g, good ? s.max_lum : 0.0);
            for (int c = 0; c < 4; c++) fprintf(out, "\t%.6g", c < s.channels ? s.sum[c] / g : 0.0);
            for (int c = 0; c < 4; c++) fprintf(out, "\t%.6g", c < s.channels && good ? s.max[c] : 0.0);
            fprintf(out, "\t%llu\t%llu\t%llu\t%llu", (unsigned long long)s.zero, (unsigned long long)s.nan,
                    (unsigned long long)s.inf, (unsigned long long)s.n);
        } else {
            for (int c = 0; c < 14; c++) fprintf(out, "\t");
        }
        fprintf(out, "\t%s\n", field(extra).c_str());
        fflush(out);
    };

    // What an event writes, and (with reads) what its shaders read.
    auto bindings = [&](uint32_t eid, bool compute, bool reads, std::vector<Out>& outs) {
        outs.clear();
        ctrl->SetFrameEvent(eid, true);
        const PipeState& ps = ctrl->GetPipelineState();
        if (!compute) {
            rdcarray<Descriptor> rts = ps.GetOutputTargets();
            for (int k = 0; k < rts.count(); k++)
                if (rts[k].resource != ResourceId())
                    outs.push_back({"RTV", rts[k].resource, rts[k].firstMip, rts[k].firstSlice, rts[k].format,
                                    "rt" + std::to_string(k)});
            Descriptor ds = ps.GetDepthTarget();
            if (ds.resource != ResourceId()) outs.push_back({"DSV", ds.resource, ds.firstMip, ds.firstSlice, ds.format, "depth"});
        }
        const ShaderStage stages[] = {ShaderStage::Vertex, ShaderStage::Pixel, ShaderStage::Compute, ShaderStage::Mesh,
                                      ShaderStage::Amplification};
        std::set<std::string> seen;
        for (ShaderStage st : stages) {
            if (compute != (st == ShaderStage::Compute)) continue;
            for (const auto& u : ps.GetReadWriteResources(st, true)) {
                if (u.descriptor.resource == ResourceId()) continue;
                std::string key = "u" + id_text(u.descriptor.resource) + ":" + std::to_string(u.descriptor.firstMip);
                if (!seen.insert(key).second) continue;
                outs.push_back({"UAV", u.descriptor.resource, u.descriptor.firstMip, u.descriptor.firstSlice,
                                u.descriptor.format, stage_name(st) + " rw " + std::to_string(u.access.index)});
            }
            if (!reads) continue;
            for (const auto& u : ps.GetReadOnlyResources(st, true)) {
                if (u.descriptor.resource == ResourceId()) continue;
                std::string key = "t" + id_text(u.descriptor.resource) + ":" + std::to_string(u.descriptor.firstMip);
                if (!seen.insert(key).second) continue;
                outs.push_back({"SRV", u.descriptor.resource, u.descriptor.firstMip, u.descriptor.firstSlice,
                                u.descriptor.format, stage_name(st) + " ro " + std::to_string(u.access.index)});
            }
        }
    };

    std::string marker;
    int pass = 0;
    std::string pass_key;
    bool open = false;
    uint32_t first = 0, last = 0, works = 0;
    std::vector<std::string> pass_programs;
    std::vector<Out> last_outs;
    std::string last_call;
    auto close_pass = [&]() {
        if (open && !each && ((only.empty() && writes.empty()) || passes)) {
            ctrl->SetFrameEvent(last, true);
            std::string programs;
            for (size_t k = 0; k < pass_programs.size(); k++) programs += (k ? "," : "") + pass_programs[k];
            char span[96];
            snprintf(span, sizeof span, "events %u-%u, %u draws/dispatches; last: ", first, last, works);
            size_t n = 0;
            for (auto& o : last_outs)
                if (strcmp(o.kind, "SRV")) row(last, pass, span + last_call, programs, "after", o, n++ < max_views ? "" : "over --max-views", n <= max_views);
            if (pass % 25 == 0) logf("pass %d measured (event %u)", pass, last);
        }
        open = false;
    };
    for (const ActionDescription* a : actions) {
        std::string name = a->customName.c_str();
        if (a->flags & ActionFlags::SetMarker) {
            if (xtp_marker(name)) marker = name;
            continue;
        }
        bool draw = (a->flags & ActionFlags::Drawcall) || (a->flags & ActionFlags::MeshDispatch);
        bool compute = bool(a->flags & ActionFlags::Dispatch);
        if (!draw && !compute) continue;
        if (a->eventId < from || a->eventId > to) {
            marker.clear();
            continue;
        }
        std::string programs = marker_programs(marker);
        std::vector<Out> outs;
        bindings(a->eventId, compute, false, outs);
        bool chosen = only.empty() && writes.empty();
        for (auto& h : only)
            if (programs.find(h) != std::string::npos) chosen = true;
        for (auto& o : outs)
            for (auto& id : writes)
                if (id_text(o.id) == id) chosen = true;
        std::string key;
        for (auto& o : outs) key += std::string("|") + o.kind + id_text(o.id) + ":" + std::to_string(o.mip);
        if (key.empty()) key = "programs " + programs;
        char args[160];
        if (compute)
            snprintf(args, sizeof args, "dispatch %ux%ux%u", a->dispatchDimension[0], a->dispatchDimension[1], a->dispatchDimension[2]);
        else
            snprintf(args, sizeof args, "draw indices %u instances %u index offset %u base vertex %d", a->numIndices,
                     a->numInstances, a->indexOffset, a->baseVertex);
        std::string label = std::string(args) + " (" + name + ") {" + marker.substr(0, 600) + "}";
        if (key != pass_key) {
            close_pass();
            pass++;
            pass_key = key;
            first = a->eventId;
            works = 0;
            pass_programs.clear();
        }
        works++;
        if (pass_programs.size() < 24 && std::find(pass_programs.begin(), pass_programs.end(), programs) == pass_programs.end())
            pass_programs.push_back(programs);
        if (inputs && chosen) {
            std::vector<Out> ins;
            bindings(a->eventId, compute, true, ins);
            ctrl->SetFrameEvent(a->eventId > 0 ? a->eventId - 1 : 0, true);
            size_t n = 0;
            for (auto& o : ins)
                if (strcmp(o.kind, "RTV")) row(a->eventId, pass, label, programs, "before", o, n++ < max_views ? "" : "over --max-views", n <= max_views);
        }
        if (depth_diff && chosen && draw) {
            for (auto& o : outs) {
                if (strcmp(o.kind, "DSV")) continue;
                std::vector<uint8_t> before, after;
                TextureDescription t{};
                DXGI_FORMAT f = DXGI_FORMAT_UNKNOWN;
                ctrl->SetFrameEvent(a->eventId > 0 ? a->eventId - 1 : 0, true);
                bool ok = read(o, before, t, f);
                ctrl->SetFrameEvent(a->eventId, true);
                ok = ok && read(o, after, t, f);
                uint64_t changed = 0;
                int channels = 0;
                int texel = texel_bytes(f, channels);
                if (ok && texel && before.size() == after.size())
                    for (size_t at = 0; at + texel <= before.size(); at += texel)
                        if (memcmp(before.data() + at, after.data() + at, std::min(texel, 4))) changed++;
                row(a->eventId, pass, label, programs, "depth-diff", o,
                    ok ? "depth texels changed: " + std::to_string((unsigned long long)changed) : "depth read failed", true);
            }
        }
        if (each && chosen) {
            ctrl->SetFrameEvent(a->eventId, true);
            for (auto& o : outs) row(a->eventId, pass, label, programs, "after", o, "", true);
        }
        last = a->eventId;
        last_outs = outs;
        last_call = label;
        open = true;
        marker.clear();
    }
    close_pass();
    fclose(out);
    ctrl->Shutdown();
    cap->Shutdown();
    logf("walk done");
    return 0;
}

int wmain(int argc, wchar_t** argv) {
    // renderdoc.dll is delay-loaded from RenderDoc's folder (XTP_RDOC_DIR,
    // else E:\tools\gpu\RenderDoc), where it finds what it injects.
    wchar_t dir[MAX_PATH] = L"E:\\tools\\gpu\\RenderDoc";
    GetEnvironmentVariableW(L"XTP_RDOC_DIR", dir, MAX_PATH);
    SetDllDirectoryW(dir);
    if (!LoadLibraryExW((std::wstring(dir) + L"\\renderdoc.dll").c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH)) {
        fprintf(stderr, "cannot load renderdoc.dll from %ls\n", dir);
        return 1;
    }
    if (argc < 2) {
        fprintf(stderr, "usage: xtp-rdoc capture|walk ... (see the head of tools/pix/xtp-rdoc.cpp)\n");
        return 2;
    }
    std::wstring cmd = argv[1];
    int rc = 2;
    if (cmd == L"capture") rc = capture(argc - 2, argv + 2);
    else if (cmd == L"walk") rc = walk(argc - 2, argv + 2);
    else fprintf(stderr, "unknown command\n");
    if (g_log) fclose(g_log);
    fflush(stdout);
    fflush(stderr);
    ExitProcess((UINT)rc);
}
