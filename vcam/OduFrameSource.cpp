//
// OpenDisplay virtual camera frame source (see OduFrameSource.h).
//
#include "pch.h"
#include "OduFrameSource.h"

namespace {
constexpr wchar_t kMappingName[] = L"Global\\OpenDisplayVCam";
}

OduFrameSource::~OduFrameSource()
{
    if (m_view)
    {
        UnmapViewOfFile(m_view);
        m_view = nullptr;
    }
    if (m_map)
    {
        CloseHandle(m_map);
        m_map = nullptr;
    }
}

void OduFrameSource::EnsureOpen()
{
    if (m_view != nullptr)
    {
        return;
    }
    // The publisher may start after the camera (or restart), so a failed open
    // is retried every couple of seconds instead of being permanent.
    const ULONGLONG now = GetTickCount64();
    if (m_tried && now - m_lastTry < 2000)
    {
        return;
    }
    m_tried = true;
    m_lastTry = now;

    HANDLE mapping = OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, FALSE, kMappingName);
    if (mapping == nullptr)
    {
        return;
    }
    void* view = MapViewOfFile(mapping, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, 0);
    if (view == nullptr)
    {
        CloseHandle(mapping);
        return;
    }
    m_map = mapping;
    m_view = static_cast<const BYTE*>(view);
}

bool OduFrameSource::CopyLatest(
    _Out_writes_bytes_(len) BYTE* dst,
    _In_ DWORD len,
    _In_ LONG pitch,
    _In_ UINT width,
    _In_ UINT height)
{
    EnsureOpen();
    if (m_view == nullptr || dst == nullptr)
    {
        return false;
    }

    const OduVCamHeader* header = reinterpret_cast<const OduVCamHeader*>(m_view);
    if (header->magic != ODU_VCAM_MAGIC || header->version != ODU_VCAM_VERSION)
    {
        return false;
    }
    if (header->width != width || header->height != height ||
        header->format != ODU_VCAM_FORMAT_NV12 ||
        header->slot_count == 0 || header->slot_size == 0)
    {
        return false;
    }
    const size_t needed = static_cast<size_t>(pitch) * height * 3 / 2;
    if (len < needed)
    {
        return false;
    }

    // A publisher may flip to another slot while we copy. With three slots a
    // rewrite of the slot being read needs two more publishes, so a couple of
    // retries is enough; a torn frame would be a visible glitch otherwise.
    for (int attempt = 0; attempt < 3; ++attempt)
    {
        const uint32_t slot = header->latest;
        if (slot >= header->slot_count)
        {
            return false;
        }
        const uint64_t seq_before = header->seq;
        const BYTE* src = m_view + ODU_VCAM_HEADER_BYTES +
                          static_cast<size_t>(slot) * header->slot_size;
        const uint32_t stride = header->stride;

        for (UINT row = 0; row < height; ++row)
        {
            memcpy(dst + static_cast<size_t>(row) * pitch,
                   src + static_cast<size_t>(row) * stride,
                   width);
        }
        const BYTE* srcUV = src + static_cast<size_t>(stride) * height;
        BYTE* dstUV = dst + static_cast<size_t>(height) * pitch;
        for (UINT row = 0; row < height / 2; ++row)
        {
            memcpy(dstUV + static_cast<size_t>(row) * pitch,
                   srcUV + static_cast<size_t>(row) * stride,
                   width);
        }
        if (header->seq == seq_before)
        {
            // Tell the sender someone is reading: it keeps the iPad camera on
            // only while this counter advances.
            const_cast<OduVCamHeader*>(header)->reads++;
            return true;
        }
    }
    return false;
}
