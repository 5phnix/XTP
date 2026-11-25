// xtp-pix: GPU captures of XTP sessions with the PIX for Windows API
// (PixApi.h, PIX 2611 or newer), and a walk of a capture's passes.
// docs/analysis/pix-world-frame.md tells what it was made for and how it
// was used. Build it with tools/pix/build.ps1; it loads pixapi.dll from the
// PIX folder at run time (XTP_PIX_DIR, else the newest one under
// C:\Program Files\Microsoft PIX).
//
//   xtp-pix capture --exe <xtp.exe> --cwd <dir> --args "<command line>"
//                   [--env K=V]... --trigger-dir <dir> [--settle-s N]
//                   [--timeout-s N] --out <file.wpix> [--log <file>]
//       Launches the exe for GPU capture, waits until <dir> holds a
//       frame-<n>.png whose n is not a power of two (XTP_CAPTURE with
//       XTP_CAPTURE_WORLD writes one once the world draws), waits N more
//       seconds, takes a one-frame capture, saves it to --out and ends the
//       process.
//   xtp-pix events <file.wpix> <out.tsv>
//       Every event of every queue: queue, index, parent, command list,
//       name and API call data. No replay.
//   xtp-pix pso <file.wpix> <out dir> <object id>...
//       Writes the shader bytecode of those pipeline state objects (ids as
//       the event list prints them, obj#<id>) as <id>-<stage>.dxil, for
//       dxc -dumpbin. No replay.
//   xtp-pix walk <file.wpix> <out.tsv> [--from N] [--to N] [--queue Q]
//                [--every-draw] [--resources <out.tsv>]
//       Replays the capture and, after each draw and dispatch in range
//       (by default only the last event before the program or the targets
//       change: one row per pass), reads every render target, depth target
//       and texture UAV bound, and writes its statistics: mean and max of
//       the luminance (and of each channel), how many texels are zero, NaN
//       or infinite.

#define NOMINMAX
#include <windows.h>
#include <shlwapi.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <filesystem>
#include <map>
#include <memory>
#include <mutex>
#include <set>
#include <string>
#include <thread>
#include <vector>

#include "d3d12-newer.h"
#include "PixApi.h"

namespace fs = std::filesystem;

// ---------------------------------------------------------------------------
// Small helpers.

static FILE* g_log = nullptr;
static std::mutex g_log_lock;

static void logf(const char* fmt, ...) {
    char buf[4096];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    SYSTEMTIME t;
    GetLocalTime(&t);
    std::lock_guard<std::mutex> g(g_log_lock);
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

static std::wstring widen(const std::string& s) {
    int n = MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, nullptr, 0);
    std::wstring w(n > 0 ? n - 1 : 0, L'\0');
    if (n > 1) MultiByteToWideChar(CP_UTF8, 0, s.c_str(), -1, w.data(), n);
    return w;
}

// Tabs and line breaks out of a field of the TSV files.
static std::string field(const char* s) {
    std::string out = s ? s : "";
    for (auto& c : out)
        if (c == '\t' || c == '\n' || c == '\r') c = ' ';
    return out;
}

template <class T>
struct Com {
    T* p = nullptr;
    Com() = default;
    Com(const Com&) = delete;
    Com& operator=(const Com&) = delete;
    Com(Com&& o) noexcept : p(o.p) { o.p = nullptr; }
    Com& operator=(Com&& o) noexcept {
        if (this != &o) {
            if (p) p->Release();
            p = o.p;
            o.p = nullptr;
        }
        return *this;
    }
    ~Com() {
        if (p) p->Release();
    }
    T* operator->() const { return p; }
    T** put() {
        if (p) p->Release();
        p = nullptr;
        return &p;
    }
    void** put_void() { return reinterpret_cast<void**>(put()); }
    explicit operator bool() const { return p != nullptr; }
};

#define CHECK(expr)                                                              \
    do {                                                                         \
        HRESULT hr_ = (expr);                                                    \
        if (FAILED(hr_)) {                                                       \
            logf("%s failed: 0x%08lx (%s:%d)", #expr, (unsigned long)hr_, __FILE__, \
                 __LINE__);                                                      \
            return 1;                                                            \
        }                                                                        \
    } while (0)

// A minimal free-threaded COM object for the PIX callback interfaces.
template <class... Ifaces>
struct Callback : Ifaces... {
    std::atomic<ULONG> refs{1};
    ULONG STDMETHODCALLTYPE AddRef() override { return ++refs; }
    ULONG STDMETHODCALLTYPE Release() override {
        ULONG r = --refs;
        if (!r) delete this;
        return r;
    }
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void** out) override {
        if (!out) return E_POINTER;
        *out = nullptr;
        if (riid == __uuidof(IUnknown)) {
            *out = static_cast<IUnknown*>(static_cast<std::tuple_element_t<0, std::tuple<Ifaces...>>*>(this));
        } else {
            (void)((riid == __uuidof(Ifaces) ? (*out = static_cast<Ifaces*>(this), true) : false) || ...);
        }
        if (!*out) return E_NOINTERFACE;
        AddRef();
        return S_OK;
    }
    virtual ~Callback() = default;
};

struct Logger final : Callback<IPixLogging> {
    void STDMETHODCALLTYPE ReportLog(PIX_LOGGING_MESSAGE m) override {
        static const char* sev[] = {"info", "warning", "error", "fatal"};
        logf("pix %s [%d %s]: %s", (unsigned)m.Severity < 4 ? sev[m.Severity] : "?", (int)m.SourceType,
             narrow(m.SourceName).c_str(), narrow(m.Message).c_str());
    }
};

struct ConnNotes final : Callback<IPixConnectionNotifications> {
    std::atomic<bool> terminated{false};
    void STDMETHODCALLTYPE FatalError(HRESULT hr) override { logf("connection: fatal error 0x%08lx", (unsigned long)hr); }
    void STDMETHODCALLTYPE AllTargetProcessesTerminated() override {
        terminated = true;
        logf("connection: all target processes terminated");
    }
    void STDMETHODCALLTYPE DeviceCounterCollectionStarted() override {}
    void STDMETHODCALLTYPE DeviceCounterCollectionStopped() override {}
    void STDMETHODCALLTYPE DeviceCounterDescriptionsUpdated() override {}
    void STDMETHODCALLTYPE NewGpuCaptureFromTarget(HRESULT hr, LPCWSTR f, const PIX_SCREENSHOT_PNG_DATA*) override {
        logf("connection: capture from target 0x%08lx %s", (unsigned long)hr, narrow(f).c_str());
    }
    void STDMETHODCALLTYPE NotifyGpuCaptureTargetProcessesStatus(UINT32 n, const PIX_TARGET_PROCESS_STATUS* s) override {
        for (UINT32 i = 0; i < n; i++)
            logf("connection: target %u %s unsupported=%d", s[i].ProcessId, narrow(s[i].ProcessName).c_str(),
                 (int)s[i].UnsupportedReason);
    }
    void STDMETHODCALLTYPE NewTimingCaptureError(HRESULT hr, LPCWSTR m) override {
        logf("connection: timing error 0x%08lx %s", (unsigned long)hr, narrow(m).c_str());
    }
    void STDMETHODCALLTYPE DeviceDisconnected(HRESULT hr) override {
        logf("connection: device disconnected 0x%08lx", (unsigned long)hr);
    }
};

struct DocNotes final : Callback<IPixGpuCaptureNotifications> {
    void STDMETHODCALLTYPE FatalError(HRESULT hr) override { logf("document: fatal error 0x%08lx", (unsigned long)hr); }
};

struct AnalysisNotes final : Callback<IPixGpuCaptureAnalysisNotifications> {
    int last = -1;
    void STDMETHODCALLTYPE OnStatus(LPCWSTR s) override { logf("analysis: %s", narrow(s).c_str()); }
    void STDMETHODCALLTYPE OnProgress(float p) override {
        int pc = (int)(p * 100);
        if (pc / 10 != last / 10) {
            last = pc;
            logf("analysis: %d %%", pc);
        }
    }
    void STDMETHODCALLTYPE OnIncompatibleDevice(LPCWSTR m) override {
        logf("analysis: incompatible device: %s", narrow(m).c_str());
    }
};

// pixapi.dll from the PIX folder.
static fs::path pix_dir() {
    wchar_t buf[MAX_PATH];
    if (GetEnvironmentVariableW(L"XTP_PIX_DIR", buf, MAX_PATH)) return buf;
    fs::path root = L"C:\\Program Files\\Microsoft PIX";
    fs::path best;
    std::error_code ec;
    for (auto& e : fs::directory_iterator(root, ec))
        if (e.is_directory() && fs::exists(e.path() / L"pixapi.dll") && e.path().filename() > best.filename())
            best = e.path();
    return best;
}

using CreateFactoryFn = HRESULT(WINAPI*)(REFIID, void**);

static int make_factory(Com<IPixFactory>& factory) {
    fs::path dir = pix_dir();
    if (dir.empty()) {
        logf("no PIX folder with pixapi.dll (set XTP_PIX_DIR)");
        return 1;
    }
    SetDllDirectoryW(dir.c_str());
    HMODULE m = LoadLibraryExW((dir / L"pixapi.dll").c_str(), nullptr, LOAD_WITH_ALTERED_SEARCH_PATH);
    if (!m) {
        logf("LoadLibrary %s failed: %lu", narrow((dir / L"pixapi.dll").c_str()).c_str(), GetLastError());
        return 1;
    }
    auto create = reinterpret_cast<CreateFactoryFn>(GetProcAddress(m, "PixCreateFactory"));
    if (!create) {
        logf("no PixCreateFactory in pixapi.dll");
        return 1;
    }
    CHECK(create(__uuidof(IPixFactory), factory.put_void()));
    logf("PIX API from %s", narrow(dir.c_str()).c_str());
    return 0;
}

// ---------------------------------------------------------------------------
// capture

static bool is_power_of_two(uint64_t n) { return n && !(n & (n - 1)); }

// A frame-<n>.png with n not a power of two: an XTP_CAPTURE_WORLD frame.
static bool world_frame_written(const fs::path& dir, std::string& name) {
    std::error_code ec;
    for (auto& e : fs::directory_iterator(dir, ec)) {
        std::string f = narrow(e.path().filename().c_str());
        uint64_t n = 0;
        char tail[16] = {};
        if (sscanf_s(f.c_str(), "frame-%llu%15s", &n, tail, (unsigned)sizeof tail) == 2 && strcmp(tail, ".png") == 0 &&
            !is_power_of_two(n)) {
            name = f;
            return true;
        }
    }
    return false;
}

static int capture(int argc, wchar_t** argv) {
    std::wstring exe, cwd, args, out, log;
    fs::path trigger;
    std::vector<std::pair<std::wstring, std::wstring>> env;
    int settle = 5, timeout = 900, capture_timeout = 600;
    for (int i = 0; i < argc; i++) {
        std::wstring a = argv[i];
        auto next = [&]() -> std::wstring { return i + 1 < argc ? argv[++i] : L""; };
        if (a == L"--exe") exe = next();
        else if (a == L"--cwd") cwd = next();
        else if (a == L"--args") args = next();
        else if (a == L"--out") out = next();
        else if (a == L"--log") log = next();
        else if (a == L"--trigger-dir") trigger = next();
        else if (a == L"--settle-s") settle = _wtoi(next().c_str());
        else if (a == L"--timeout-s") timeout = _wtoi(next().c_str());
        else if (a == L"--capture-timeout-s") capture_timeout = _wtoi(next().c_str());
        else if (a == L"--env") {
            std::wstring kv = next();
            size_t eq = kv.find(L'=');
            if (eq == std::wstring::npos) {
                logf("--env %s: expected K=V", narrow(kv.c_str()).c_str());
                return 2;
            }
            env.emplace_back(kv.substr(0, eq), kv.substr(eq + 1));
        } else {
            logf("capture: unknown argument %s", narrow(a.c_str()).c_str());
            return 2;
        }
    }
    if (exe.empty() || out.empty() || trigger.empty()) {
        logf("capture needs --exe, --out and --trigger-dir");
        return 2;
    }
    if (!log.empty()) _wfopen_s(&g_log, log.c_str(), L"a");

    Com<IPixFactory> factory;
    if (make_factory(factory)) return 1;
    auto* logger = new Logger();
    factory->SetLogger(logger);

    PIX_CONNECTION_DESC_LOCAL local{PIX_CONNECTION_FLAG_LOCAL_NONE};
    PIX_CONNECTION_DESC desc{};
    desc.Type = PIX_CONNECTION_TYPE_LOCAL;
    desc.pLocal = &local;
    auto* notes = new ConnNotes();
    Com<IPixConnectionDocument> conn;
    CHECK(factory->OpenConnectionDocument(&desc, notes, logger, __uuidof(IPixConnectionDocument), conn.put_void()));
    CHECK(conn->Connect(nullptr));

    std::vector<PIX_STRING_PAIR> pairs;
    for (auto& [k, v] : env) pairs.push_back({k.c_str(), v.c_str()});
    PIX_LAUNCH_PROCESS_DESC launch{};
    launch.LaunchMode = PIX_APPLICATION_LAUNCH_MODE_WIN32_EXECUTABLE;
    launch.LaunchFlags = PIX_APPLICATION_LAUNCH_FLAG_UNDER_GPU_CAPTURE | PIX_APPLICATION_LAUNCH_FLAG_GPU_CAPTURE_DISABLE_HUD;
    launch.LaunchInfo.Win32.ExecutablePath = exe.c_str();
    launch.LaunchInfo.Win32.InitialWorkingDirectory = cwd.empty() ? nullptr : cwd.c_str();
    launch.LaunchInfo.Win32.CommandLineArguments = args.c_str();
    launch.LaunchInfo.Win32.EnvironmentVariableCount = (UINT32)pairs.size();
    launch.LaunchInfo.Win32.EnvironmentVariables = pairs.empty() ? nullptr : pairs.data();
    Com<IPixLaunchProcessResults> launched;
    CHECK(conn->LaunchProcess(&launch, __uuidof(IPixLaunchProcessResults), launched.put_void()));
    UINT32 pid = launched->GetProcessId();
    logf("launched %s pid %u (unsupported=%d)", narrow(launched->GetProcessName()).c_str(), pid,
         (int)launched->GetUnsupportedReason());
    printf("pid %u\n", pid);
    fflush(stdout);

    HANDLE proc = OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE, FALSE, pid);
    auto alive = [&]() { return proc && WaitForSingleObject(proc, 0) == WAIT_TIMEOUT; };
    auto end_target = [&]() {
        conn->DetachFromAllProcesses(TRUE);
        if (alive()) TerminateProcess(proc, 1);
    };

    // Wait for the world.
    auto start = std::chrono::steady_clock::now();
    std::string seen;
    while (!world_frame_written(trigger, seen)) {
        if (!alive() || notes->terminated) {
            logf("the target ended before the world frame appeared in %s", narrow(trigger.c_str()).c_str());
            end_target();
            return 3;
        }
        if (std::chrono::steady_clock::now() - start > std::chrono::seconds(timeout)) {
            logf("no world frame in %s after %d s", narrow(trigger.c_str()).c_str(), timeout);
            end_target();
            return 4;
        }
        Sleep(1000);
    }
    logf("world frame %s seen after %lld s; settling %d s", seen.c_str(),
         (long long)std::chrono::duration_cast<std::chrono::seconds>(std::chrono::steady_clock::now() - start).count(),
         settle);
    Sleep(settle * 1000);

    PIX_GPU_CAPTURE_OPTIONS options{PIX_GPU_CAPTURE_DELIMITER_PRESENT, 1, PIX_GPU_CAPTURE_KEY_NONE, pid};
    CHECK(conn->SetGpuCaptureOptions(&options));
    Com<IPixCancellationToken> cancel;
    CHECK(factory->CreateCancellationToken(__uuidof(IPixCancellationToken), cancel.put_void()));
    std::atomic<bool> done{false};
    std::thread watchdog([&]() {
        for (int s = 0; s < capture_timeout && !done; s++) Sleep(1000);
        if (!done) {
            logf("capture still running after %d s: cancelling", capture_timeout);
            cancel->Cancel();
        }
    });
    PIX_GPU_CAPTURE_DESC cd{pid};
    Com<IPixGpuCaptureResult> result;
    logf("taking a one-frame GPU capture");
    HRESULT hr = conn->TakeGpuCapture(&cd, cancel.p, __uuidof(IPixGpuCaptureResult), result.put_void());
    done = true;
    watchdog.join();
    if (FAILED(hr)) {
        logf("TakeGpuCapture failed: 0x%08lx", (unsigned long)hr);
        end_target();
        return 5;
    }
    std::wstring file = result->GetFilename();
    logf("capture written: %s (flags %d)", narrow(file.c_str()).c_str(), (int)result->GetFlags());
    end_target();
    std::error_code ec;
    fs::create_directories(fs::path(out).parent_path(), ec);
    fs::rename(file, out, ec);
    if (ec) {
        ec.clear();
        fs::copy_file(file, out, fs::copy_options::overwrite_existing, ec);
        if (ec) {
            logf("copying %s to %s: %s", narrow(file.c_str()).c_str(), narrow(out.c_str()).c_str(),
                 ec.message().c_str());
            return 6;
        }
        fs::remove(file, ec);
    }
    PIX_SCREENSHOT_PNG_DATA png{};
    result->GetScreenshotPngData(&png);
    if (png.Data && png.Size) {
        fs::path shot = fs::path(out).replace_extension(L".png");
        FILE* f = nullptr;
        if (!_wfopen_s(&f, shot.c_str(), L"wb") && f) {
            fwrite(png.Data, 1, png.Size, f);
            fclose(f);
        }
    }
    logf("saved %s (%llu bytes)", narrow(out.c_str()).c_str(), (unsigned long long)fs::file_size(out, ec));
    return 0;
}

// ---------------------------------------------------------------------------
// events and walk: defined in the second half (see below).

static int events(int argc, wchar_t** argv);
static int walk(int argc, wchar_t** argv);
static int pso(int argc, wchar_t** argv);

int wmain(int argc, wchar_t** argv) {
    CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    if (argc < 2) {
        fprintf(stderr, "usage: xtp-pix capture|events|walk ... (see the head of tools/pix/xtp-pix.cpp)\n");
        return 2;
    }
    std::wstring cmd = argv[1];
    int rc = 2;
    if (cmd == L"capture") rc = capture(argc - 2, argv + 2);
    else if (cmd == L"events") rc = events(argc - 2, argv + 2);
    else if (cmd == L"walk") rc = walk(argc - 2, argv + 2);
    else if (cmd == L"pso") rc = pso(argc - 2, argv + 2);
    else fprintf(stderr, "unknown command\n");
    if (g_log) fclose(g_log);
    // pixapi's worker threads may still hold the process: leave at once.
    fflush(stdout);
    fflush(stderr);
    ExitProcess((UINT)rc);
}

// ---------------------------------------------------------------------------
// Opening a capture.

static int open_capture(const std::wstring& file, Com<IPixFactory>& factory, Com<IPixGpuCaptureDocument>& doc) {
    if (make_factory(factory)) return 1;
    auto* logger = new Logger();
    factory->SetLogger(logger);
    CHECK(factory->OpenGpuCaptureDocument(file.c_str(), new DocNotes(), logger, __uuidof(IPixGpuCaptureDocument),
                                          doc.put_void()));
    return 0;
}

struct Queue {
    Com<IPixGpuCaptureQueueInfo> info;
    UINT32 id;
    std::string name;
    UINT32 count;
};

static int queues(IPixGpuCaptureDocument* doc, std::vector<Queue>& out) {
    Com<IPixCollection> qs;
    CHECK(doc->GetQueues(__uuidof(IPixCollection), qs.put_void()));
    for (UINT64 i = 0; i < qs->GetCount(); i++) {
        Queue q;
        CHECK(qs->Get(i, __uuidof(IPixGpuCaptureQueueInfo), q.info.put_void()));
        q.id = q.info->GetId();
        q.name = narrow(q.info->GetName());
        q.count = q.info->GetEventCount();
        out.push_back(std::move(q));
    }
    return 0;
}

static int events(int argc, wchar_t** argv) {
    if (argc < 2) {
        logf("events <file.wpix> <out.tsv>");
        return 2;
    }
    Com<IPixFactory> factory;
    Com<IPixGpuCaptureDocument> doc;
    if (open_capture(argv[0], factory, doc)) return 1;
    std::vector<Queue> qs;
    if (queues(doc.p, qs)) return 1;
    FILE* f = nullptr;
    if (_wfopen_s(&f, argv[1], L"w") || !f) {
        logf("cannot write %s", narrow(argv[1]).c_str());
        return 1;
    }
    fprintf(f, "queue\tindex\tparent\tlist\tgpu_id\tname\tcall\n");
    for (auto& q : qs) {
        logf("queue %u %s: %u events", q.id, q.name.c_str(), q.count);
        for (UINT32 i = 0; i < q.count; i++) {
            PIX_EVENT_INFO e{};
            if (FAILED(q.info->GetEvent(i, &e))) continue;
            fprintf(f, "%u\t%u\t%u\t%u\t%u\t%s\t%s\n", q.id, e.Index, e.ParentIndex, e.CommandListId, e.GpuId,
                    field(e.Name).c_str(), field(e.ApiCallData).c_str());
        }
    }
    fclose(f);
    return 0;
}

#include "walk.inl"
