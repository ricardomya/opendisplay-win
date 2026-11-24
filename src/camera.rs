//! iPad camera as a Windows webcam.
//!
//! The receiver captures with AVCaptureSession, encodes H.264 and sends
//! `ODCM` frames over the existing TCP session. Here they are decoded with the
//! in-box Media Foundation H.264 decoder into NV12, published into a named
//! shared section, and a Media Foundation virtual camera (the small COM media
//! source DLL in `vcam/`) serves them to Teams/Zoom/Chrome and the Windows
//! camera stack.
//!
//! The media source runs in the camera Frame Server (`LocalService`), so the
//! section is created with a DACL that grants it access, and the DLL is
//! registered machine-wide once (`install-camera`).

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tracing::{debug, info, warn};
use windows::core::{GUID, HSTRING, Interface, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
};
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFCreateVirtualCamera, MFIsVirtualCameraTypeSupported, MFStartup,
    MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource, MFMediaType_Video, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_OUTPUT_DATA_BUFFER, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION, CODECAPI_AVLowLatencyMode,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
};

use crate::latest::{self, Sender as LatestSender};

/// CLSID of our media source (vcam/, sample-derived). Keep in sync with
/// vcam/VirtualCameraMediaSource.h.
pub const VCAM_CLSID: &str = "{9A1D3C2E-6B4F-4E7A-9C8D-2F5B7A1C4E60}";

/// Flip the camera picture horizontally (tray menu). Read per frame so the
/// switch takes effect immediately, without reconnecting.
pub static MIRROR: AtomicBool = AtomicBool::new(false);

const MAGIC: u32 = 0x4F44_5643; // 'ODVC'
const VERSION: u32 = 1;
const FORMAT_NV12: u32 = 1;
const SLOTS: u32 = 3;
const HEADER_BYTES: usize = 64;
const MAPPING_NAME: &str = "Global\\OpenDisplayVCam";

const MF_E_TRANSFORM_NEED_MORE_INPUT: i32 = 0xC00D_6D72u32 as i32;
const MF_E_TRANSFORM_STREAM_CHANGE: i32 = 0xC00D_6D61u32 as i32;

/// One encoded access unit handed from the session to the camera worker.
struct EncodedFrame {
    data: Vec<u8>,
    keyframe: bool,
}

pub struct CameraSession {
    stop: Arc<AtomicBool>,
    tx: Option<LatestSender<EncodedFrame>>,
    handle: Option<thread::JoinHandle<()>>,
    /// Frames decoded, for the heartbeat log.
    decoded: Arc<AtomicU64>,
}

impl CameraSession {
    /// Queue an Annex B access unit (SPS/PPS in-band on keyframes).
    pub fn push(&self, data: &[u8], keyframe: bool) {
        if let Some(tx) = &self.tx {
            tx.send(EncodedFrame {
                data: data.to_vec(),
                keyframe,
            });
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tx.take(); // closes the channel, waking the worker
        if let Some(h) = self.handle.take() { let _ = h.join(); }
    }
}

impl Drop for CameraSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tx.take();
    }
}

/// True when the media source DLL is registered and its file exists.
pub fn is_registered() -> bool {
    let subkey = HSTRING::from(format!(
        "SOFTWARE\\Classes\\CLSID\\{VCAM_CLSID}\\InProcServer32"
    ));
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if ok.0 != 0 {
        return false;
    }
    let len = (size as usize / 2).saturating_sub(1);
    let path = String::from_utf16_lossy(&buf[..len]);
    !path.is_empty() && std::path::Path::new(&path).exists()
}

/// Start the virtual camera and its decode/publish worker. Returns None when
/// the feature is disabled (`OD_NO_CAMERA`) or the media source is not
/// registered yet.
pub fn start(width: u32, height: u32) -> Option<CameraSession> {
    if std::env::var_os("OD_NO_CAMERA").is_some() {
        info!("camera: disabled by OD_NO_CAMERA");
        return None;
    }
    if !is_registered() {
        warn!("camera: media source is not registered; run `install-camera`");
        return None;
    }

    let (tx, rx) = latest::channel::<EncodedFrame>();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let decoded = Arc::new(AtomicU64::new(0));
    let decoded_thread = decoded.clone();
    let handle = thread::Builder::new()
        .name("camera".into())
        .spawn(move || run(rx, stop_thread, width, height, decoded_thread))
        .expect("spawn camera thread");
    Some(CameraSession {
        stop,
        tx: Some(tx),
        handle: Some(handle),
        decoded,
    })
}

fn run(
    rx: latest::Receiver<EncodedFrame>,
    stop: Arc<AtomicBool>,
    width: u32,
    height: u32,
    decoded: Arc<AtomicU64>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, 0);
    }

    let host = match VcamHost::start() {
        Ok(h) => h,
        Err(e) => {
            warn!("camera: cannot start the virtual camera: {e:#}");
            return;
        }
    };
    let mut publisher = match Publisher::new(width, height) {
        Ok(p) => p,
        Err(e) => {
            warn!("camera: shared memory: {e:#}");
            let _ = host.stop();
            return;
        }
    };
    let mut decoder = match Decoder::new(width, height) {
        Ok(d) => d,
        Err(e) => {
            warn!("camera: H.264 decoder: {e:#}");
            let _ = host.stop();
            return;
        }
    };
    info!("camera: virtual camera ready ({width}x{height}, waiting for iPad frames)");

    let mut frames_in: u64 = 0;
    let mut errors: u64 = 0;
    let mut last_report = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let frame = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => {
                if last_report.elapsed() >= Duration::from_secs(15) && frames_in > 0 {
                    last_report = Instant::now();
                    info!(
                        "camera: {frames_in} frame(s) decoded, {errors} error(s), {} published",
                        publisher.published()
                    );
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        frames_in += 1;
        if frame.keyframe {
            debug!("camera: keyframe, {} bytes", frame.data.len());
        }
        match decoder.decode(&frame.data, &mut |sample, width, height| {
            if let Err(e) = publisher.publish_sample(sample, width, height) {
                warn!("camera: publish: {e}");
            }
        }) {
            Ok(n) => {
                if n > 0 {
                    decoded.fetch_add(n as u64, Ordering::Relaxed);
                }
            }
            Err(e) => {
                errors += 1;
                if errors == 1 || errors % 100 == 0 {
                    warn!("camera: decode failed ({errors}): {e}");
                }
                // A broken reference chain recovers at the next IDR; recreate
                // the decoder so a stuck transform cannot persist.
                if let Ok(d) = Decoder::new(width, height) {
                    decoder = d;
                }
            }
        }
        if last_report.elapsed() >= Duration::from_secs(15) && frames_in > 0 {
            last_report = Instant::now();
            info!(
                "camera: {frames_in} frame(s) decoded, {errors} error(s), {} published",
                publisher.published()
            );
        }
    }

    let _ = host.stop();
    debug!("camera: stopped");
}

// --- named shared section -------------------------------------------------

struct Publisher {
    mapping: HANDLE,
    view: *mut u8,
    mapped: MEMORY_MAPPED_VIEW_ADDRESS,
    width: u32,
    height: u32,
    stride: u32,
    slot_size: usize,
    seq: u64,
    published: u64,
    scratch: Vec<u8>,
}



impl Publisher {
    fn new(width: u32, height: u32) -> Result<Self> {
        let stride = width;
        let slot_size = (stride * height * 3 / 2) as usize;
        let total = HEADER_BYTES + SLOTS as usize * slot_size;

        // The Frame Server runs as LOCAL SERVICE in session 0; it is not in the
        // default DACL of a section created by the user, so grant it (with
        // SYSTEM and the owner) explicitly.
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let sddl = HSTRING::from("D:(A;;GA;;;SY)(A;;GA;;;LS)(A;;GA;;;OW)");
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .context("security descriptor")?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(&sa),
                PAGE_READWRITE,
                0,
                total as u32,
                &HSTRING::from(MAPPING_NAME),
            )
        };
        unsafe {
            let _ = LocalFree(Some(HLOCAL(sd.0)));
        }
        let mapping = mapping.context("CreateFileMappingW")?;
        if mapping.is_invalid() {
            return Err(anyhow!("CreateFileMappingW returned an invalid handle"));
        }
        let mapped = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, total) };
        if mapped.Value.is_null() {
            unsafe {
                let _ = CloseHandle(mapping);
            }
            return Err(anyhow!("MapViewOfFile failed"));
        }
        let view = mapped.Value as *mut u8;

        let put32 = |off: usize, v: u32| unsafe {
            std::ptr::write_unaligned(view.add(off) as *mut u32, v);
        };
        put32(0, MAGIC);
        put32(4, VERSION);
        put32(8, width);
        put32(12, height);
        put32(16, stride);
        put32(20, FORMAT_NV12);
        put32(24, SLOTS);
        put32(28, slot_size as u32);
        unsafe {
            std::ptr::write_unaligned(view.add(32) as *mut u64, 0);
            std::ptr::write_unaligned(view.add(40) as *mut u32, 0);
            std::ptr::write_unaligned(view.add(48) as *mut u64, 0);
        }

        Ok(Self {
            mapping,
            view,
            mapped,
            width,
            height,
            stride,
            slot_size,
            seq: 0,
            published: 0,
            scratch: Vec::new(),
        })
    }

    fn published(&self) -> u64 {
        self.published
    }

    /// Copy a decoded NV12 frame, cropping/scaling it to the media source's
    /// fixed 1280x720 type (the iPad may negotiate 4:3 or another size).
    fn publish_sample(&mut self, sample: &IMFSample, src_w: u32, src_h: u32) -> Result<()> {
        let buffer: IMFMediaBuffer = unsafe { sample.ConvertToContiguousBuffer()? };
        if src_w < 2 || src_h < 2 {
            return Err(anyhow!("bad NV12 size {src_w}x{src_h}"));
        }

        // Prefer the 2D interface: it reports the real row pitch.
        let b2d = buffer.cast::<windows::Win32::Media::MediaFoundation::IMF2DBuffer>();
        if let Ok(b2d) = b2d {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut pitch = 0i32;
            if unsafe { b2d.Lock2D(&mut ptr, &mut pitch) }.is_ok() {
                let result = if ptr.is_null() || pitch <= 0 {
                    Err(anyhow!("bad 2D NV12 buffer"))
                } else {
                    let len = pitch as usize * src_h as usize * 3 / 2;
                    let data = unsafe { std::slice::from_raw_parts(ptr, len) };
                    self.publish_data(data, pitch as usize, src_w, src_h);
                    Ok(())
                };
                let _ = unsafe { b2d.Unlock2D() };
                if result.is_ok() {
                    return result;
                }
            }
        }

        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut max = 0u32;
        let mut cur = 0u32;
        unsafe {
            buffer.Lock(&mut ptr, Some(&mut max), Some(&mut cur))?;
        }
        let len = cur as usize;
        let needed = (src_w * src_h * 3 / 2) as usize;
        let result = if ptr.is_null() || len < needed {
            Err(anyhow!("NV12 buffer too small: {len} < {needed}"))
        } else {
            let data = unsafe { std::slice::from_raw_parts(ptr, len) };
            self.publish_data(data, src_w as usize, src_w, src_h);
            Ok(())
        };
        unsafe {
            let _ = buffer.Unlock();
        }
        result
    }

    fn publish_data(&mut self, src: &[u8], stride: usize, src_w: u32, src_h: u32) {
        let (dw, dh) = (self.width as usize, self.height as usize);
        let mirror = MIRROR.load(Ordering::Relaxed);
        if stride == dw && src_w as usize == dw && src_h as usize == dh && !mirror {
            self.publish(src);
            return;
        }
        let mut out = std::mem::take(&mut self.scratch);
        out.resize(dw * dh * 3 / 2, 0);
        if stride == dw && src_w as usize == dw && src_h as usize == dh {
            let slot = dw * dh * 3 / 2;
            if src.len() >= slot {
                out.copy_from_slice(&src[..slot]);
            }
        } else {
            normalize_nv12(src, stride, src_w as usize, src_h as usize, &mut out, dw, dh);
        }
        if mirror {
            flip_nv12(&mut out, dw, dh);
        }
        self.publish(&out);
        self.scratch = out;
    }

    fn publish(&mut self, nv12: &[u8]) {
        let slot = (self.seq % SLOTS as u64) as usize;
        let dst = unsafe { self.view.add(HEADER_BYTES + slot * self.slot_size) };
        unsafe {
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), dst, self.slot_size);
            std::ptr::write_unaligned(self.view.add(40) as *mut u32, slot as u32);
            std::ptr::write_unaligned(
                self.view.add(48) as *mut u64,
                self.seq.wrapping_mul(333_333),
            );
            (*(self.view.add(32) as *mut AtomicU64)).store(self.seq + 1, Ordering::Release);
        }

        self.seq += 1;
        self.published += 1;
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.mapped);
            let _ = CloseHandle(self.mapping);
        }
    }
}

/// Center-crop NV12 to the destination aspect and nearest-neighbour scale it
/// to `dw`x`dh`. `stride` is the source row pitch in bytes (Y and UV rows
/// share it, as NV12 does).
fn normalize_nv12(
    src: &[u8],
    stride: usize,
    sw: usize,
    sh: usize,
    dst: &mut [u8],
    dw: usize,
    dh: usize,
) {
    let sw = sw & !1; // even, for 4:2:0 chroma
    let sh = sh & !1;
    if sw < 2 || sh < 2 || dw < 2 || dh < 2 || dst.len() < dw * dh * 3 / 2 {
        return;
    }
    let (mut cw, mut ch) = (sw, sh);
    if sw * dh > sh * dw {
        cw = (sh * dw / dh) & !1;
    } else if sw * dh < sh * dw {
        ch = (sw * dh / dw) & !1;
    }
    if cw < 2 || ch < 2 {
        return;
    }
    let ox = (sw - cw) / 2;
    let oy = (sh - ch) / 2;

    let (dst_y, dst_uv) = dst[..dw * dh * 3 / 2].split_at_mut(dw * dh);
    for ty in 0..dh {
        let sy = oy + ty * ch / dh;
        let row = &src[sy * stride + ox..sy * stride + ox + cw];
        let drow = &mut dst_y[ty * dw..(ty + 1) * dw];
        for tx in 0..dw {
            drow[tx] = row[tx * cw / dw];
        }
    }

    let src_uv = &src[stride * sh..];
    let uoy = oy / 2;
    let uch = ch / 2;
    let uh = dh / 2;
    for ty in 0..uh {
        let sy = uoy + ty * uch / uh;
        let row = &src_uv[sy * stride + ox..sy * stride + ox + cw];
        let drow = &mut dst_uv[ty * dw..(ty + 1) * dw];
        for tx in 0..dw {
            drow[tx] = row[tx * cw / dw];
        }
    }
}

/// Mirror an NV12 image left-to-right (Y rows reversed, UV pairs reversed in
/// place so U and V stay paired).
fn flip_nv12(buf: &mut [u8], width: usize, height: usize) {
    let width = width & !1;
    if width < 2 || height < 2 || buf.len() < width * height * 3 / 2 {
        return;
    }
    for row in 0..height {
        buf[row * width..(row + 1) * width].reverse();
    }
    let uv = width * height;
    for row in 0..height / 2 {
        let line = &mut buf[uv + row * width..uv + (row + 1) * width];
        let mut left = 0;
        let mut right = width - 2;
        while left < right {
            line.swap(left, right);
            line.swap(left + 1, right + 1);
            left += 2;
            right -= 2;
        }
    }
}

// --- Media Foundation H.264 decoder --------------------------------------

struct Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    out_width: u32,
    out_height: u32,
}

impl Decoder {
    fn new(width: u32, height: u32) -> Result<Self> {
        let transform: IMFTransform = unsafe {
            CoCreateInstance(
                &GUID::from_u128(0x62CE7E72_4C71_4D20_B15D_452831A87D9D),
                None,
                CLSCTX_ALL,
            )
            .context("CLSID_CMSH264DecoderMFT")?
        };
        unsafe {
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
            }
            let input = media_type(&MFMediaType_Video, &MFVideoFormat_H264, width, height)?;
            transform.SetInputType(0, &input, 0)?;
            let output = media_type(&MFMediaType_Video, &MFVideoFormat_NV12, width, height)?;
            transform.SetOutputType(0, &output, 0)?;
            let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        }
        Ok(Self {
            transform,
            width,
            height,
            out_width: width,
            out_height: height,
        })
    }

    /// After a stream change pick the decoder's NV12 type at the stream's real
    /// size (the iPad may send 4:3 or another size, not the 16:9 we asked for).
    fn select_output_type(&mut self) -> Result<()> {
        unsafe {
            for index in 0..16 {
                let Ok(mt) = self.transform.GetOutputAvailableType(0, index) else {
                    break;
                };
                if mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or_default() != MFVideoFormat_NV12 {
                    continue;
                }
                let Ok(size) = mt.GetUINT64(&MF_MT_FRAME_SIZE) else {
                    continue;
                };
                let width = (size >> 32) as u32;
                let height = (size & 0xFFFF_FFFF) as u32;
                if width < 2 || height < 2 {
                    continue;
                }
                self.transform.SetOutputType(0, &mt, 0)?;
                self.out_width = width;
                self.out_height = height;
                return Ok(());
            }
        }
        Err(anyhow!("the decoder offers no NV12 output type"))
    }

    /// Feed one Annex B access unit; call `out` for every decoded NV12 frame.
    fn decode(
        &mut self,
        annexb: &[u8],
        out: &mut dyn FnMut(&IMFSample, u32, u32),
    ) -> Result<usize> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(annexb.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(annexb.as_ptr(), ptr, annexb.len());
            let _ = buffer.Unlock();
            buffer.SetCurrentLength(annexb.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            if let Err(e) = self.transform.ProcessInput(0, &sample, 0) {
                return Err(anyhow!("ProcessInput: {e}"));
            }

            let mut produced = 0usize;
            loop {
                let out_buffer = MFCreateMemoryBuffer(self.out_width * self.out_height * 3 / 2 + 4096)?;
                let out_sample = MFCreateSample()?;
                out_sample.AddBuffer(&out_buffer)?;
                let mut data = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample)),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                };
                let mut status = 0u32;
                let result = self
                    .transform
                    .ProcessOutput(0, std::slice::from_mut(&mut data), &mut status);
                // The MFT hands our provided sample back on every outcome,
                // including MF_E_TRANSFORM_NEED_MORE_INPUT. `ManuallyDrop`
                // skips the field's destructor, so reclaim it here or every
                // decoded frame leaks its 1.4 MB NV12 buffer (the 146 GB
                // runaway that ended in an out-of-memory abort).
                let sample = ManuallyDrop::take(&mut data.pSample);
                let _events = ManuallyDrop::take(&mut data.pEvents);
                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            produced += 1;
                            out(&sample, self.out_width, self.out_height);
                        }
                    }
                    Err(e) if e.code().0 == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) if e.code().0 == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.select_output_type()?;
                        continue;
                    }
                    Err(e) => return Err(anyhow!("ProcessOutput: {e}")),
                }
            }
            Ok(produced)
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
        }
    }
}

fn media_type(major: &GUID, sub: &GUID, width: u32, height: u32) -> Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, major)?;
        mt.SetGUID(&MF_MT_SUBTYPE, sub)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, 2)?; // progressive
        Ok(mt)
    }
}

// --- virtual camera lifetime ---------------------------------------------

struct VcamHost {
    camera: windows::Win32::Media::MediaFoundation::IMFVirtualCamera,
}

impl VcamHost {
    fn start() -> Result<Self> {
        unsafe {
            match MFIsVirtualCameraTypeSupported(MFVirtualCameraType_SoftwareCameraSource) {
                Ok(supported) if supported.as_bool() => {}
                _ => return Err(anyhow!("software virtual cameras are not supported")),
            }
            let name = HSTRING::from("OpenDisplay Camera");
            let source = HSTRING::from(VCAM_CLSID);
            let camera = MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &name,
                &source,
                None,
            )
            .context("MFCreateVirtualCamera")?;
            camera.Start(None).context("IMFVirtualCamera::Start")?;
            Ok(Self { camera })
        }
    }

    fn stop(self) -> Result<()> {
        unsafe {
            let _ = self.camera.Stop();
            let _ = self.camera.Remove();
        }
        Ok(())
    }
}

impl Drop for VcamHost {
    fn drop(&mut self) {
        unsafe {
            let _ = self.camera.Stop();
            let _ = self.camera.Remove();
        }
    }
}

// --- one-time installer ---------------------------------------------------

const VCAM_DLL_NAME: &str = "VirtualCameraMediaSource.dll";
const VCAM_DIR: &str = r"C:\ProgramData\OpenDisplay\VCam";

/// Copy the media source DLL to a machine-readable place and register it
/// (asks for administrator rights).
pub fn install() -> Result<()> {
    crate::driver::elevate_or("install-camera", install_elevated)
}

pub fn uninstall() -> Result<()> {
    crate::driver::elevate_or("uninstall-camera", uninstall_elevated)
}

fn dll_source() -> Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("OD_VCAM_DLL") {
        let path = std::path::PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        return Err(anyhow!("OD_VCAM_DLL points at a missing file"));
    }
    let exe = std::env::current_exe()?;
    let beside = exe.parent().unwrap_or(std::path::Path::new(".")).join(VCAM_DLL_NAME);
    if beside.exists() {
        return Ok(beside);
    }
    Err(anyhow!(
        "{VCAM_DLL_NAME} must sit next to opendisplay-win.exe (build it from vcam/), or set OD_VCAM_DLL"
    ))
}

fn install_elevated() -> Result<()> {
    let source = dll_source()?;
    let dir = std::path::Path::new(VCAM_DIR);
    std::fs::create_dir_all(dir).context("create camera directory")?;
    let dest = dir.join(VCAM_DLL_NAME);
    std::fs::copy(&source, &dest).context("copy media source DLL")?;
    register_dll(&dest)?;
    println!(
        "Camera support installed: {}\nRun opendisplay-win.exe (or the tray app) and the camera appears in Windows Settings while a receiver is connected.",
        dest.display()
    );
    Ok(())
}

fn register_dll(path: &std::path::Path) -> Result<()> {
    let key = format!(r"HKLM\SOFTWARE\Classes\CLSID\{VCAM_CLSID}\InProcServer32");
    let path = path.to_string_lossy().to_string();
    let status = std::process::Command::new("reg")
        .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", &path, "/f"])
        .status()
        .context("run reg add")?;
    if !status.success() {
        return Err(anyhow!("reg add failed for {key}"));
    }
    let status = std::process::Command::new("reg")
        .args([
            "add",
            &key,
            "/v",
            "ThreadingModel",
            "/t",
            "REG_SZ",
            "/d",
            "Both",
            "/f",
        ])
        .status()
        .context("run reg add ThreadingModel")?;
    if !status.success() {
        return Err(anyhow!("reg add ThreadingModel failed"));
    }
    Ok(())
}

fn uninstall_elevated() -> Result<()> {
    let key = format!(r"HKLM\SOFTWARE\Classes\CLSID\{VCAM_CLSID}");
    let status = std::process::Command::new("reg")
        .args(["delete", &key, "/f"])
        .status()
        .context("run reg delete")?;
    if !status.success() {
        eprintln!("note: the CLSID key was not present");
    }
    let _ = std::fs::remove_dir_all(VCAM_DIR);
    println!("Camera support removed.");
    Ok(())
}
