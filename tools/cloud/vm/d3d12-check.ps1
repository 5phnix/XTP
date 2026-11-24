<#
  Runs ON THE VM (setup-vm.ps1 and run-remote.ps1 -DeviceCheck call it).
  Windows PowerShell 5.1.

  Lists the DXGI adapters and, for each, what a D3D12 device on it supports:
  feature level, highest shader model, mesh shader tier, wave lanes and
  ray tracing tier. XTP creates its device on the default adapter
  (D3D12CreateDevice with no adapter, i.e. adapter 0) and needs shader model
  6.6 with mesh shaders for the world; the last line says whether adapter 0
  qualifies. Also prints nvidia-smi's GPU, driver and licence lines.
  Exits 1 when adapter 0 is software or lacks SM 6.6 / mesh shaders.
#>
$ErrorActionPreference = 'Stop'
$source = @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;

public static class XtpD3D12Check {
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    struct DXGI_ADAPTER_DESC1 {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string Description;
        public uint VendorId, DeviceId, SubSysId, Revision;
        public UIntPtr DedicatedVideoMemory, DedicatedSystemMemory, SharedSystemMemory;
        public uint LuidLow; public int LuidHigh; public uint Flags;
    }
    [DllImport("dxgi.dll")] static extern int CreateDXGIFactory1(ref Guid riid, out IntPtr factory);
    [DllImport("d3d12.dll")] static extern int D3D12CreateDevice(IntPtr adapter, int minLevel, ref Guid riid, out IntPtr device);

    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate uint ReleaseFn(IntPtr self);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int EnumAdapters1Fn(IntPtr self, uint index, out IntPtr adapter);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int GetDesc1Fn(IntPtr self, out DXGI_ADAPTER_DESC1 desc);
    [UnmanagedFunctionPointer(CallingConvention.StdCall)] delegate int CheckFeatureSupportFn(IntPtr self, int feature, IntPtr data, uint size);

    static T Method<T>(IntPtr obj, int slot) where T : class {
        IntPtr vtbl = Marshal.ReadIntPtr(obj);
        return Marshal.GetDelegateForFunctionPointer(Marshal.ReadIntPtr(vtbl, slot * IntPtr.Size), typeof(T)) as T;
    }
    static void Release(IntPtr obj) { if (obj != IntPtr.Zero) Method<ReleaseFn>(obj, 2)(obj); }

    // IDXGIFactory1::EnumAdapters1 = 12, IDXGIAdapter1::GetDesc1 = 10,
    // ID3D12Device::CheckFeatureSupport = 13 (vtable slots).
    static int Check(IntPtr dev, int feature, int[] data) {
        var h = GCHandle.Alloc(data, GCHandleType.Pinned);
        try { return Method<CheckFeatureSupportFn>(dev, 13)(dev, feature, h.AddrOfPinnedObject(), (uint)(data.Length * 4)); }
        finally { h.Free(); }
    }

    public static List<string> Run(out bool adapter0Ok) {
        var lines = new List<string>();
        adapter0Ok = false;
        Guid iidFactory = new Guid("770aae78-f26f-4dba-a829-253c83d1b387");
        Guid iidDevice = new Guid("189819f1-1db6-4b57-be54-1821339b85f7");
        IntPtr factory;
        int hr = CreateDXGIFactory1(ref iidFactory, out factory);
        if (hr < 0) { lines.Add(string.Format("CreateDXGIFactory1 failed 0x{0:x8}", hr)); return lines; }
        for (uint i = 0; ; i++) {
            IntPtr adapter;
            if (Method<EnumAdapters1Fn>(factory, 12)(factory, i, out adapter) < 0) break;
            DXGI_ADAPTER_DESC1 d;
            Method<GetDesc1Fn>(adapter, 10)(adapter, out d);
            bool software = (d.Flags & 2) != 0;
            string head = string.Format("adapter {0}: {1} (vendor 0x{2:x4} device 0x{3:x4}, {4:N0} MB dedicated{5})",
                i, d.Description, d.VendorId, d.DeviceId, (ulong)d.DedicatedVideoMemory / (1024 * 1024), software ? ", SOFTWARE" : "");
            IntPtr dev;
            hr = D3D12CreateDevice(adapter, 0xb000, ref iidDevice, out dev);
            if (hr < 0) { lines.Add(head + string.Format(": no D3D12 device (0x{0:x8})", hr)); Release(adapter); continue; }
            // D3D12_FEATURE_FEATURE_LEVELS (2) needs a pointer; ask level by level instead.
            string fl = "?";
            int[] levels = { 0xc200, 0xc100, 0xc000, 0xb100, 0xb000 };
            foreach (int lv in levels) {
                IntPtr probe;
                if (D3D12CreateDevice(adapter, lv, ref iidDevice, out probe) >= 0) {
                    Release(probe); fl = string.Format("{0}_{1}", lv >> 12, (lv >> 8) & 0xf); break;
                }
            }
            // D3D12_FEATURE_SHADER_MODEL (7): ask for 6.9, the runtime lowers it to what it supports.
            int sm = 0;
            for (int want = 0x69; want >= 0x60 && sm == 0; want--) {
                var data = new int[] { want };
                if (Check(dev, 7, data) >= 0) sm = data[0];
            }
            var o7 = new int[2];   // D3D12_FEATURE_D3D12_OPTIONS7 (32): MeshShaderTier, SamplerFeedbackTier
            int mesh = Check(dev, 32, o7) >= 0 ? o7[0] : -1;
            var o1 = new int[6];   // D3D12_FEATURE_D3D12_OPTIONS1 (8): WaveOps, lane count min, max, ...
            bool o1ok = Check(dev, 8, o1) >= 0;
            var o5 = new int[3];   // D3D12_FEATURE_D3D12_OPTIONS5 (27): ..., RaytracingTier
            int rt = Check(dev, 27, o5) >= 0 ? o5[2] : -1;
            lines.Add(head);
            lines.Add(string.Format("    feature level {0}, shader model {1}.{2}, mesh shader tier {3}, wave lanes {4}, ray tracing tier {5}",
                fl, sm >> 4, sm & 0xf, mesh == 0 ? "none" : mesh == 10 ? "1" : mesh.ToString(),
                o1ok ? string.Format("{0}-{1}", o1[1], o1[2]) : "?", rt <= 0 ? "none" : string.Format("{0}.{1}", rt / 10, rt % 10)));
            if (i == 0) adapter0Ok = !software && sm >= 0x66 && mesh >= 10;
            Release(dev);
            Release(adapter);
        }
        Release(factory);
        return lines;
    }
}
'@
Add-Type -TypeDefinition $source
$ok = $false
[XtpD3D12Check]::Run([ref]$ok) | ForEach-Object { Write-Output $_ }
$smi = @('C:\Windows\System32\nvidia-smi.exe', 'C:\Program Files\NVIDIA Corporation\NVSMI\nvidia-smi.exe') | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($smi) {
    $ErrorActionPreference = 'Continue'
    & $smi --query-gpu=name,driver_version,driver_model.current,memory.total --format=csv,noheader 2>&1 | ForEach-Object { "nvidia-smi: $_" }
    & $smi -q 2>&1 | Select-String -Pattern 'License Status|Product Name|vGPU Software Licensed Product' | ForEach-Object { "nvidia-smi:$($_.Line)" }
    $ErrorActionPreference = 'Stop'
}
if ($ok) {
    Write-Output 'XTP device check: OK (adapter 0 is hardware with shader model 6.6+ and mesh shaders)'
    exit 0
}
Write-Output 'XTP device check: FAILED (adapter 0, the one XTP uses, lacks hardware, shader model 6.6 or mesh shaders)'
exit 1
