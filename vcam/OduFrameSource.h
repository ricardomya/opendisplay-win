//
// OpenDisplay virtual camera frame source.
//
// The media source DLL runs inside the Windows camera Frame Server. The
// OpenDisplay sender (a normal user process) publishes decoded NV12 frames
// into a named shared-memory mapping; this class copies the latest one into
// each sample the frame server asks for. When nothing is published yet (iPad
// not connected, camera starting), the caller draws a simple test pattern.
//
#pragma once

#include <cstdint>

// Must match src/camera/publisher.rs on the Rust side.
constexpr uint32_t ODU_VCAM_MAGIC = 0x4F445643; // 'ODVC'
constexpr uint32_t ODU_VCAM_VERSION = 1;
constexpr uint32_t ODU_VCAM_FORMAT_NV12 = 1;
constexpr uint32_t ODU_VCAM_SLOTS = 3;
constexpr uint32_t ODU_VCAM_HEADER_BYTES = 64;

#pragma pack(push, 1)
struct OduVCamHeader {
    uint32_t magic;
    uint32_t version;
    uint32_t width;
    uint32_t height;
    uint32_t stride;      // bytes per Y row; UV rows use the same stride
    uint32_t format;      // ODU_VCAM_FORMAT_NV12
    uint32_t slot_count;
    uint32_t slot_size;   // stride * height * 3 / 2
    volatile uint64_t seq;    // incremented by the publisher per frame
    volatile uint32_t latest; // index of the newest slot
    uint32_t reserved;
    uint64_t qpc;         // 100 ns timestamp of the newest frame
};
#pragma pack(pop)

static_assert(sizeof(OduVCamHeader) <= ODU_VCAM_HEADER_BYTES, "header grew past its slot area");

class OduFrameSource
{
public:
    OduFrameSource() = default;
    ~OduFrameSource();

    OduFrameSource(const OduFrameSource&) = delete;
    OduFrameSource& operator=(const OduFrameSource&) = delete;

    // Copy the newest NV12 frame into an MF sample buffer. False when no
    // valid frame is available (caller falls back to the test pattern).
    bool CopyLatest(_Out_writes_bytes_(len) BYTE* dst, _In_ DWORD len, _In_ LONG pitch,
                    _In_ UINT width, _In_ UINT height);

private:
    void EnsureOpen();

    HANDLE m_map = nullptr;
    const BYTE* m_view = nullptr;
    bool m_tried = false;
    ULONGLONG m_lastTry = 0;
};
