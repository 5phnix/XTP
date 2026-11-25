// The five types of a newer d3d12.h (the Agility SDK's) that PIX 2611's API
// headers name and the Windows SDK 10.0.26100 lacks. xtp-pix never reads
// them: they sit in parts of the API it does not use (dump files, the
// application identity, D3D settings, PIX_RESOURCE_DESC's pointer), so
// stand-ins of the right kind (enum, struct, incomplete struct) are enough
// to compile. Do not use these declarations for anything else.
#pragma once
#include <d3d12.h>

#ifndef XTP_PIX_HAS_NEWER_D3D12
enum D3D12_DEVICE_ERROR_CODE : UINT32 {};
enum D3D12_DUMP_FILE_DRIVER_OPTIONS : UINT32 {};
struct D3D12_FEATURE_DATA_DUMP_FILE {
    UINT32 unused;
};
struct D3D12_APPLICATION_DESC {
    const wchar_t* unused;
};
struct D3D12_RESOURCE_DESC2;
#endif
