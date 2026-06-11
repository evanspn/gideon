//! Kobo e-ink framebuffer backend.
//!
//! Kobo devices expose their e-ink panel as a Linux framebuffer (`/dev/fb0`)
//! driven by an i.MX EPDC. Drawing is two steps: write pixels into the mmap'd
//! framebuffer, then ask the EPDC to refresh a region with the
//! `MXCFB_SEND_UPDATE` ioctl.
//!
//! This backend targets the common 8bpp grayscale configuration KOReader
//! also uses. It is only compiled with the `kobo` feature and only works on
//! Linux; CI cross-checks it for `armv7-unknown-linux-musleabihf`.

#![cfg(target_os = "linux")]
#![allow(non_camel_case_types)]

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;

use gideon_render::{GrayPage, RgbPage};
use memmap2::MmapMut;

use crate::{blit_into, blit_rgb_into, Display, Error, RefreshMode, Result};

// --- Linux fb + mxcfb ABI (from linux/fb.h and mxcfb.h) ---

const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOGET_FSCREENINFO: u32 = 0x4602;
// _IOW('F', 0x2E, struct mxcfb_update_data_v1_ntx) — pre-Mark 7 kernels
// (the NTX alt-buffer layout includes virt_addr): 0x44 bytes on the
// 32-bit device ABI. Value matches KOReader's generated bindings.
const MXCFB_SEND_UPDATE_V1: u32 = 0x4044462E;
// _IOW('F', 0x2E, struct mxcfb_update_data_v2) — Mark 7+: dither fields
// added and the alt-buffer DROPS virt_addr: 0x48 bytes on the device ABI.
const MXCFB_SEND_UPDATE_V2: u32 = 0x4048462E;
// _IOW('F', 0x2E, struct hwtcon_update_data) — MTK devices (Libra Colour,
// Clara BW/Colour, Elipsa 2E): a different driver (HWTCON) with a compact
// 36-byte update struct.
const HWTCON_SEND_UPDATE: u32 = 0x4024462E;
// _IOWR('F', 0x2F, struct hwtcon_update_marker_data) — wait for a sent
// update to complete; KOReader waits on flashing refreshes so MTK updates
// don't stack and glitch the panel.
const HWTCON_WAIT_FOR_UPDATE_COMPLETE: u32 = 0xC008462F;
// _IOW('F', 0x37, uint32_t) — KOReader waits for submission of the marker
// it just sent on Kobo MTK ("wait_for_submission_after = true").
const HWTCON_WAIT_FOR_UPDATE_SUBMISSION: u32 = 0x40044637;
// MTK REAGL: KOReader's partial-refresh waveform on HWTCON devices
// ("REAGL is *always* available" there).
const HWTCON_WAVEFORM_MODE_GLR16: u32 = 4;
// Kaleido color GC16: mtk-kobo.h's HWTCON_WAVEFORM_MODE_GCC16 = 10 ("Used
// for images on color panels"), and like everything Kaleido-tweaked it is
// *always* paired with UPDATE_MODE_FULL. KOReader uses it as
// `waveform_color` for flashing refreshes of color content.
const HWTCON_WAVEFORM_MODE_GCC16: u32 = 10;
const FBIOPUT_VSCREENINFO: u32 = 0x4601;
// Mark 7 dithering: passthrough (off).
const EPDC_FLAG_USE_DITHERING_PASSTHROUGH: i32 = 0x0;

/// `libc::ioctl`'s request parameter is `c_ulong` on glibc but `c_int` on
/// 32-bit musl (the Kobo target); this wrapper papers over the difference.
///
/// # Safety
/// Same contract as `libc::ioctl`: `arg` must match what `request` expects.
unsafe fn ioctl<T>(fd: libc::c_int, request: u32, arg: *mut T) -> libc::c_int {
    libc::ioctl(fd, request as _, arg)
}

const WAVEFORM_MODE_AUTO: u32 = 0x101;
/// NTX GC16: the full-quality 16-gray waveform KOReader uses for flashes.
const WAVEFORM_MODE_GC16: u32 = 2;
const UPDATE_MODE_PARTIAL: u32 = 0x0;
const UPDATE_MODE_FULL: u32 = 0x1;
const TEMP_USE_AMBIENT: i32 = 0x1000;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct fb_bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct fb_var_screeninfo {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: fb_bitfield,
    green: fb_bitfield,
    blue: fb_bitfield,
    transp: fb_bitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct fb_fix_screeninfo {
    id: [u8; 16],
    smem_start: libc::c_ulong,
    smem_len: u32,
    type_: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    line_length: u32,
    mmio_start: libc::c_ulong,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

impl Default for fb_fix_screeninfo {
    fn default() -> Self {
        // SAFETY: plain-old-data struct, all-zeroes is a valid value.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct mxcfb_rect {
    top: u32,
    left: u32,
    width: u32,
    height: u32,
}

/// Pre-Mark 7 (NTX) alt-buffer layout: includes virt_addr.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct mxcfb_alt_buffer_data_ntx {
    virt_addr: libc::c_ulong,
    phys_addr: u32,
    width: u32,
    height: u32,
    alt_update_region: mxcfb_rect,
}

/// Mark 7+ alt-buffer layout: virt_addr was dropped.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct mxcfb_alt_buffer_data {
    phys_addr: u32,
    width: u32,
    height: u32,
    alt_update_region: mxcfb_rect,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct mxcfb_update_data_v1 {
    update_region: mxcfb_rect,
    waveform_mode: u32,
    update_mode: u32,
    update_marker: u32,
    temp: i32,
    flags: u32,
    alt_buffer_data: mxcfb_alt_buffer_data_ntx,
}

/// Mark 7+ layout (KOReader's `mxcfb_update_data_v2`): v1 plus the
/// hardware-dithering fields, inserted before `alt_buffer_data`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct mxcfb_update_data_v2 {
    update_region: mxcfb_rect,
    waveform_mode: u32,
    update_mode: u32,
    update_marker: u32,
    temp: i32,
    flags: u32,
    dither_mode: i32,
    quant_bit: i32,
    alt_buffer_data: mxcfb_alt_buffer_data,
}

/// MTK wait payload: marker + (unimplemented) collision test.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct hwtcon_update_marker_data {
    update_marker: u32,
    collision_test: u32,
}

/// MTK/HWTCON update payload (Libra Colour & friends): no temp, no
/// alt-buffer — just region, waveform, mode, marker, flags, dither.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct hwtcon_update_data {
    update_region: mxcfb_rect,
    waveform_mode: u32,
    update_mode: u32,
    update_marker: u32,
    flags: u32,
    dither_mode: i32,
}

/// Which refresh ioctl dialect the kernel speaks; probed on the first
/// refresh and cached. MTK first (newest devices), then mxcfb v2, then v1.
#[derive(Clone, Copy, PartialEq)]
enum UpdateApi {
    Mtk,
    V2,
    V1,
}

/// E-ink framebuffer display on Kobo hardware.
pub struct KoboDisplay {
    file: File,
    map: MmapMut,
    width: u32,
    height: u32,
    line_length: u32,
    bytes_per_pixel: u32,
    update_marker: u32,
    update_api: Option<UpdateApi>,
    /// The rotation the kernel actually settled on (0..=3) — kernels may
    /// refuse the normalization, and the touch transform must follow the
    /// settled value, not the requested one.
    rotate: u32,
    /// The upright rotation we asked for (`upright_rotate_for_product`,
    /// or the `GIDEON_FB_ROTATE` override).
    upright_rotate: u32,
    /// Whether the last blit carried real color: a FULL refresh then uses
    /// the Kaleido color waveform (GCC16) on MTK kernels.
    last_blit_color: bool,
}

impl KoboDisplay {
    /// Open the default framebuffer device.
    pub fn open() -> Result<Self> {
        Self::open_path("/dev/fb0")
    }

    pub fn open_path(path: &str) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;

        let mut var = fb_var_screeninfo::default();
        let mut fix = fb_fix_screeninfo::default();
        // SAFETY: standard framebuffer ioctls on a freshly opened fb device,
        // passing properly sized zero-initialized out-structs.
        unsafe {
            if ioctl(file.as_raw_fd(), FBIOGET_VSCREENINFO, &mut var) != 0 {
                return Err(Error::Display(format!(
                    "FBIOGET_VSCREENINFO failed on {path}: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO, &mut fix) != 0 {
                return Err(Error::Display(format!(
                    "FBIOGET_FSCREENINFO failed on {path}: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        // KOReader normalizes the framebuffer rotation to upright at startup
        // (fbdepth -R UR); device touch tables assume that orientation.
        // Crucially "upright" is NOT rotate=0 on every device — the native
        // rotate value that scans out a linear buffer upright is
        // per-device (see [`upright_rotate_for_product`]). Best effort: a
        // kernel that refuses keeps its rotation and we adapt to whatever
        // geometry it reports.
        let wanted_rotate = upright_rotate_from_env();
        if var.rotate != wanted_rotate {
            let mut wanted = var;
            wanted.rotate = wanted_rotate;
            // SAFETY: FBIOPUT_VSCREENINFO with a struct obtained from the
            // matching GET, only the rotate field changed.
            let ret = unsafe { ioctl(file.as_raw_fd(), FBIOPUT_VSCREENINFO, &mut wanted) };
            // Re-read whatever the kernel settled on (geometry may have
            // changed even on partial success).
            unsafe {
                let _ = ioctl(file.as_raw_fd(), FBIOGET_VSCREENINFO, &mut var);
                let _ = ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO, &mut fix);
            }
            eprintln!(
                "gideon fb: rotation normalize to {wanted_rotate} {} (now rotate={})",
                if ret == 0 && var.rotate == wanted_rotate {
                    "ok"
                } else {
                    "refused"
                },
                var.rotate
            );
        }
        eprintln!(
            "gideon fb: {}x{} {}bpp rotate={} line_length={} smem_len={}",
            var.xres, var.yres, var.bits_per_pixel, var.rotate, fix.line_length, fix.smem_len
        );

        // Stock Nickel leaves the panel at 16 or 32bpp; we convert grayscale
        // to whatever depth the framebuffer reports instead of requiring a
        // depth switch.
        if !matches!(var.bits_per_pixel, 8 | 16 | 24 | 32) {
            return Err(Error::Display(format!(
                "unsupported framebuffer depth: {}bpp (supported: 8/16/24/32)",
                var.bits_per_pixel
            )));
        }

        // /dev/fb0 is a character device with file length 0 — the mapping
        // length must come from the driver's advertised memory size, not
        // the file metadata.
        let needed = fix.line_length as usize * var.yres as usize;
        let map_len = fix.smem_len as usize;
        if map_len == 0 || needed == 0 {
            return Err(Error::Display(format!(
                "framebuffer reports zero memory size (smem_len={} line_length={} yres={})",
                fix.smem_len, fix.line_length, var.yres
            )));
        }
        if needed > map_len {
            return Err(Error::Display(format!(
                "framebuffer memory too small: need {needed} bytes (line_length={} x yres={}), driver advertises {map_len}",
                fix.line_length, var.yres
            )));
        }
        // SAFETY: mapping exactly the framebuffer memory the kernel
        // advertised via FBIOGET_FSCREENINFO.
        let map = unsafe { memmap2::MmapOptions::new().len(map_len).map_mut(&file)? };

        Ok(Self {
            file,
            map,
            width: var.xres,
            height: var.yres,
            line_length: fix.line_length,
            bytes_per_pixel: var.bits_per_pixel / 8,
            update_marker: 0,
            update_api: None,
            rotate: var.rotate,
            upright_rotate: wanted_rotate,
            last_blit_color: false,
        })
    }

    /// The rotation the framebuffer actually settled on (0..=3). When the
    /// kernel refused our normalization this differs from
    /// [`Self::upright_rotation`], and the touch transform must be rotated
    /// by the difference.
    pub fn rotation(&self) -> u32 {
        self.rotate
    }

    /// The upright rotation this device's touch tables assume (the
    /// `upright_rotate_for_product` value used, or the `GIDEON_FB_ROTATE`
    /// override).
    pub fn upright_rotation(&self) -> u32 {
        self.upright_rotate
    }
}

/// The native `rotate` value that scans a linear framebuffer out *upright*
/// (canonical portrait, what the UI and the touch tables assume).
///
/// This is NOT 0 on every device: each Kobo panel is mounted with its own
/// scanout origin. The values mirror FBInk's empirically verified
/// per-device rotation maps (`fbink_device_id.c`), which is exactly what
/// KOReader's `fbdepth -R UR` startup normalization resolves through:
///
/// * Libra Colour (`monza*`) and Elipsa 2E (`condor`): canonical UR is
///   native rotate=1 (map {UR:1, CW:0, UD:3, CCW:2}). Under Nickel the
///   Libra Colour sits at rotate=3 — upside-down for a linear renderer.
/// * Clara BW/Colour (`spa*`): canonical UR is native rotate=3
///   (map {UR:3, CW:2, UD:1, CCW:0}).
/// * Unknown devices keep the historical best-effort rotate=0.
fn upright_rotate_for_product(product: Option<&str>) -> u32 {
    match product.map(|p| p.trim().to_ascii_lowercase()).as_deref() {
        Some("monza") | Some("monzakobo") | Some("monzatolino") | Some("condor") => 1,
        Some(p) if p.starts_with("spa") => 3,
        _ => 0,
    }
}

/// Resolve the upright rotation: `GIDEON_FB_ROTATE` (0..=3) overrides,
/// otherwise the per-device default for the Kobo `PRODUCT` codename (set
/// by the stock system and re-derived by our launcher when missing).
fn upright_rotate_from_env() -> u32 {
    if let Some(rotate) = std::env::var("GIDEON_FB_ROTATE")
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|r| *r <= 3)
    {
        return rotate;
    }
    upright_rotate_for_product(std::env::var("PRODUCT").ok().as_deref())
}

impl Display for KoboDisplay {
    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn blit(&mut self, page: &GrayPage, offset_y: u32) -> Result<()> {
        // Render into a contiguous grayscale buffer first, then convert to
        // the framebuffer's pixel format row by row, honoring line stride.
        let mut staging = vec![0xFFu8; (self.width * self.height) as usize];
        blit_into(&mut staging, self.width, self.height, page, offset_y);

        let bpp = self.bytes_per_pixel as usize;
        for y in 0..self.height as usize {
            let src_row = &staging[y * self.width as usize..(y + 1) * self.width as usize];
            let dst = y * self.line_length as usize;
            let dst_row = &mut self.map[dst..dst + self.width as usize * bpp];
            write_gray_row(src_row, dst_row, bpp);
        }
        self.last_blit_color = false;
        Ok(())
    }

    fn blit_rgb(&mut self, page: &RgbPage, offset_y: u32) -> Result<()> {
        // Same staging dance as `blit`, but the color survives all the way
        // into the framebuffer (the Kaleido panel's CFA does the rest).
        let mut staging = vec![0xFFu8; (self.width * self.height * 3) as usize];
        blit_rgb_into(&mut staging, self.width, self.height, page, offset_y);

        let bpp = self.bytes_per_pixel as usize;
        for y in 0..self.height as usize {
            let src_row = &staging[y * self.width as usize * 3..(y + 1) * self.width as usize * 3];
            let dst = y * self.line_length as usize;
            let dst_row = &mut self.map[dst..dst + self.width as usize * bpp];
            write_rgb_row(src_row, dst_row, bpp);
        }
        self.last_blit_color = true;
        Ok(())
    }

    fn flush(&mut self, mode: RefreshMode) -> Result<()> {
        // No msync here: framebuffer mappings are device memory — writes
        // are immediately visible to the EPDC, and msync on a character
        // device returns EINVAL on Kobo kernels (KOReader never syncs the
        // fb mapping either).

        self.update_marker = self.update_marker.wrapping_add(1).max(1);
        let region = mxcfb_rect {
            top: 0,
            left: 0,
            width: self.width,
            height: self.height,
        };
        // KOReader's Kobo configuration: GC16 for flashing refreshes, AUTO
        // for partials.
        let (update_mode, waveform) = match mode {
            RefreshMode::Full => (UPDATE_MODE_FULL, WAVEFORM_MODE_GC16),
            RefreshMode::Partial => (UPDATE_MODE_PARTIAL, WAVEFORM_MODE_AUTO),
        };

        // Kernel generations disagree on the update struct (KOReader
        // handles this per-device; we probe). Try the cached variant, or
        // V2 (Mark 7+, every Kobo since ~2018) then V1 on the first call.
        let candidates: &[UpdateApi] = match self.update_api {
            Some(UpdateApi::Mtk) => &[UpdateApi::Mtk],
            Some(UpdateApi::V2) => &[UpdateApi::V2],
            Some(UpdateApi::V1) => &[UpdateApi::V1],
            None => &[UpdateApi::Mtk, UpdateApi::V2, UpdateApi::V1],
        };

        let mut last_err = None;
        for &api in candidates {
            let ret = match api {
                UpdateApi::Mtk => {
                    // KOReader's HWTCON config: GC16 flashes, REAGL (GLR16)
                    // partials — and REAGL must ALWAYS be paired with
                    // UPDATE_MODE_FULL on these kernels (it doesn't flash;
                    // see mtk-kobo.h and KOReader's hard promotion). When
                    // the framebuffer holds real color (an RGB blit), the
                    // flash uses the Kaleido color waveform instead.
                    let mtk_waveform = match mode {
                        RefreshMode::Full if self.last_blit_color => HWTCON_WAVEFORM_MODE_GCC16,
                        RefreshMode::Full => WAVEFORM_MODE_GC16,
                        RefreshMode::Partial => HWTCON_WAVEFORM_MODE_GLR16,
                    };
                    let mut update = hwtcon_update_data {
                        update_region: region,
                        waveform_mode: mtk_waveform,
                        update_mode: UPDATE_MODE_FULL,
                        update_marker: self.update_marker,
                        flags: 0,
                        dither_mode: 0,
                    };
                    // SAFETY: fully initialized struct matching the ioctl.
                    unsafe { ioctl(self.file.as_raw_fd(), HWTCON_SEND_UPDATE, &mut update) }
                }
                UpdateApi::V2 => {
                    let mut update = mxcfb_update_data_v2 {
                        update_region: region,
                        waveform_mode: waveform,
                        update_mode,
                        update_marker: self.update_marker,
                        temp: TEMP_USE_AMBIENT,
                        dither_mode: EPDC_FLAG_USE_DITHERING_PASSTHROUGH,
                        quant_bit: 0,
                        ..Default::default()
                    };
                    // SAFETY: fully initialized struct matching the ioctl.
                    unsafe { ioctl(self.file.as_raw_fd(), MXCFB_SEND_UPDATE_V2, &mut update) }
                }
                UpdateApi::V1 => {
                    let mut update = mxcfb_update_data_v1 {
                        update_region: region,
                        waveform_mode: waveform,
                        update_mode,
                        update_marker: self.update_marker,
                        temp: TEMP_USE_AMBIENT,
                        ..Default::default()
                    };
                    // SAFETY: fully initialized struct matching the ioctl.
                    unsafe { ioctl(self.file.as_raw_fd(), MXCFB_SEND_UPDATE_V1, &mut update) }
                }
            };
            if ret == 0 {
                self.update_api = Some(api);
                // MTK: wait for flashing refreshes to finish (KOReader does)
                // so back-to-back updates can't stack; best-effort.
                if api == UpdateApi::Mtk {
                    // KOReader on Kobo MTK waits for submission of the
                    // just-sent marker after every update…
                    let mut submitted: u32 = self.update_marker;
                    // SAFETY: uint32 payload per the ioctl definition.
                    let _ = unsafe {
                        ioctl(
                            self.file.as_raw_fd(),
                            HWTCON_WAIT_FOR_UPDATE_SUBMISSION,
                            &mut submitted,
                        )
                    };
                    // …and for completion: every MTK send above is
                    // UPDATE_MODE_FULL (REAGL requirement), and KOReader
                    // waits for completion after FULL updates.
                    {
                        let mut marker = hwtcon_update_marker_data {
                            update_marker: self.update_marker,
                            collision_test: 0,
                        };
                        // SAFETY: fully initialized marker struct.
                        let _ = unsafe {
                            ioctl(
                                self.file.as_raw_fd(),
                                HWTCON_WAIT_FOR_UPDATE_COMPLETE,
                                &mut marker,
                            )
                        };
                    }
                }
                return Ok(());
            }
            last_err = Some(std::io::Error::last_os_error());
        }
        Err(Error::Display(format!(
            "screen refresh rejected (tried MTK+v2+v1, {}x{}, mode={update_mode}): {}",
            self.width,
            self.height,
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown".to_string())
        )))
    }
}

/// Convert one row of 8-bit grayscale into the framebuffer pixel format.
/// Gray means R = G = B, so channel order (RGB vs BGR) doesn't matter.
fn write_gray_row(gray: &[u8], out: &mut [u8], bytes_per_pixel: usize) {
    match bytes_per_pixel {
        1 => out[..gray.len()].copy_from_slice(gray),
        2 => {
            // RGB565: gray -> (g>>3, g>>2, g>>3), little-endian.
            for (i, &g) in gray.iter().enumerate() {
                let v = ((g as u16 >> 3) << 11) | ((g as u16 >> 2) << 5) | (g as u16 >> 3);
                out[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        3 => {
            for (i, &g) in gray.iter().enumerate() {
                out[i * 3..i * 3 + 3].copy_from_slice(&[g, g, g]);
            }
        }
        4 => {
            // XRGB/XBGR with opaque alpha/padding byte.
            for (i, &g) in gray.iter().enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&[g, g, g, 0xFF]);
            }
        }
        _ => {}
    }
}

/// Convert one row of packed RGB (3 bytes per pixel) into the framebuffer
/// pixel format. Kobo color framebuffers are BGRA — blue lands in byte 0;
/// grayscale (8bpp) framebuffers get the Rec.601 luma.
fn write_rgb_row(rgb: &[u8], out: &mut [u8], bytes_per_pixel: usize) {
    match bytes_per_pixel {
        1 => {
            for (i, px) in rgb.chunks_exact(3).enumerate() {
                out[i] = gideon_render::luma_rec601(px[0], px[1], px[2]);
            }
        }
        2 => {
            // RGB565, little-endian: r in the top 5 bits.
            for (i, px) in rgb.chunks_exact(3).enumerate() {
                let v =
                    ((px[0] as u16 >> 3) << 11) | ((px[1] as u16 >> 2) << 5) | (px[2] as u16 >> 3);
                out[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        3 => {
            // 24bpp BGR.
            for (i, px) in rgb.chunks_exact(3).enumerate() {
                out[i * 3..i * 3 + 3].copy_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        4 => {
            // 32bpp BGRA with opaque alpha.
            for (i, px) in rgb.chunks_exact(3).enumerate() {
                out[i * 4..i * 4 + 4].copy_from_slice(&[px[2], px[1], px[0], 0xFF]);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod waveform_tests {
    use super::*;

    #[test]
    fn kaleido_color_waveform_is_gcc16_equals_10() {
        // Cross-checked against KOReader's bindings (koreader-base
        // ffi/mxcfb_kobo_h.lua: HWTCON_WAVEFORM_MODE_GCC16 = 10) and the
        // kernel header (mtk-kobo.h HWTCON_WAVEFORM_MODE_ENUM: GCC16 = 10,
        // "Used for images on color panels"). Pin it so a typo can't
        // silently turn color refreshes into something else.
        assert_eq!(HWTCON_WAVEFORM_MODE_GCC16, 10);
        // And its grayscale neighbors, for completeness.
        assert_eq!(WAVEFORM_MODE_GC16, 2);
        assert_eq!(HWTCON_WAVEFORM_MODE_GLR16, 4);
    }
}

#[cfg(test)]
mod pixel_format_tests {
    use super::{write_gray_row, write_rgb_row};

    #[test]
    fn gray_to_8bpp_is_identity() {
        let mut out = [0u8; 3];
        write_gray_row(&[0x00, 0x80, 0xFF], &mut out, 1);
        assert_eq!(out, [0x00, 0x80, 0xFF]);
    }

    #[test]
    fn gray_to_rgb565() {
        let mut out = [0u8; 6];
        write_gray_row(&[0x00, 0xFF, 0x80], &mut out, 2);
        // Black -> 0x0000, white -> 0xFFFF.
        assert_eq!(&out[0..2], &0x0000u16.to_le_bytes());
        assert_eq!(&out[2..4], &0xFFFFu16.to_le_bytes());
        // Mid gray: r=0x80>>3=16, g=0x80>>2=32, b=16 -> equal-ish channels.
        let v = u16::from_le_bytes([out[4], out[5]]);
        assert_eq!(v >> 11, 16);
        assert_eq!((v >> 5) & 0x3F, 32);
        assert_eq!(v & 0x1F, 16);
    }

    #[test]
    fn gray_to_32bpp_replicates_channels_with_opaque_alpha() {
        let mut out = [0u8; 8];
        write_gray_row(&[0x12, 0xAB], &mut out, 4);
        assert_eq!(out, [0x12, 0x12, 0x12, 0xFF, 0xAB, 0xAB, 0xAB, 0xFF]);
    }

    #[test]
    fn gray_to_24bpp() {
        let mut out = [0u8; 6];
        write_gray_row(&[0x10, 0xF0], &mut out, 3);
        assert_eq!(out, [0x10, 0x10, 0x10, 0xF0, 0xF0, 0xF0]);
    }

    #[test]
    fn rgb_to_32bpp_is_bgra_with_blue_in_byte_0() {
        // Kobo framebuffers are BGRA: red must land in byte 2, blue in 0.
        let mut out = [0u8; 8];
        write_rgb_row(&[0xFF, 0x00, 0x00, 0x12, 0x34, 0x56], &mut out, 4);
        assert_eq!(out, [0x00, 0x00, 0xFF, 0xFF, 0x56, 0x34, 0x12, 0xFF]);
    }

    #[test]
    fn rgb_to_rgb565() {
        let mut out = [0u8; 6];
        write_rgb_row(&[0xFF, 0x00, 0x00, 0x00, 0xFF, 0x00], &mut out, 2);
        // Pure red -> 0xF800, pure green -> 0x07E0 (little-endian).
        assert_eq!(&out[0..2], &0xF800u16.to_le_bytes());
        assert_eq!(&out[2..4], &0x07E0u16.to_le_bytes());
    }

    #[test]
    fn rgb_to_8bpp_uses_rec601_luma() {
        let mut out = [0u8; 3];
        write_rgb_row(&[255, 0, 0, 0, 255, 0, 0, 0, 255], &mut out, 1);
        assert_eq!(out, [76, 150, 29]);
    }

    #[test]
    fn rgb_to_24bpp_is_bgr() {
        let mut out = [0u8; 3];
        write_rgb_row(&[0x11, 0x22, 0x33], &mut out, 3);
        assert_eq!(out, [0x33, 0x22, 0x11]);
    }
}

#[cfg(test)]
mod rotation_tests {
    use super::upright_rotate_for_product;

    #[test]
    fn libra_colour_family_is_upright_at_rotate_1() {
        // FBInk rotationMap for monza/condor: canonical UR == native 1.
        assert_eq!(upright_rotate_for_product(Some("monza")), 1);
        assert_eq!(upright_rotate_for_product(Some(" MonzaKobo ")), 1);
        assert_eq!(upright_rotate_for_product(Some("monzaTolino")), 1);
        assert_eq!(upright_rotate_for_product(Some("condor")), 1);
    }

    #[test]
    fn clara_family_is_upright_at_rotate_3() {
        // FBInk rotationMap for spa*: canonical UR == native 3.
        assert_eq!(upright_rotate_for_product(Some("spaBW")), 3);
        assert_eq!(upright_rotate_for_product(Some("spaColour")), 3);
        assert_eq!(upright_rotate_for_product(Some("spaTolinoBW")), 3);
    }

    #[test]
    fn unknown_devices_keep_the_historical_zero() {
        assert_eq!(upright_rotate_for_product(Some("frost")), 0);
        assert_eq!(upright_rotate_for_product(None), 0);
    }
}

#[cfg(test)]
mod ioctl_encoding_tests {
    use super::*;

    /// _IOW('F', 0x2E, T) per the Linux ioctl encoding.
    fn iow_f_2e(size: usize) -> u32 {
        (1u32 << 30) | ((size as u32) << 16) | (0x46 << 8) | 0x2E
    }

    #[test]
    fn update_ioctl_numbers_encode_the_32bit_struct_sizes() {
        // v1 (NTX layout, includes virt_addr) is 0x44 bytes on the 32-bit
        // device ABI; v2 (dither fields, no virt_addr) is 0x48. Values
        // match KOReader's generated bindings.
        assert_eq!(iow_f_2e(0x44), MXCFB_SEND_UPDATE_V1);
        assert_eq!(iow_f_2e(0x48), MXCFB_SEND_UPDATE_V2);
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(std::mem::size_of::<mxcfb_update_data_v1>(), 0x44);
            assert_eq!(std::mem::size_of::<mxcfb_update_data_v2>(), 0x48);
        }
        // v2's struct has no pointer-width fields anymore: pin it here too.
        assert_eq!(std::mem::size_of::<mxcfb_update_data_v2>(), 0x48);
    }

    #[test]
    fn hwtcon_ioctl_matches_struct_size_on_all_arches() {
        // hwtcon_update_data is all fixed-width fields: 36 bytes everywhere.
        assert_eq!(std::mem::size_of::<hwtcon_update_data>(), 0x24);
        assert_eq!(iow_f_2e(0x24), HWTCON_SEND_UPDATE);
    }

    #[test]
    fn hwtcon_wait_ioctl_matches_struct_size() {
        assert_eq!(std::mem::size_of::<hwtcon_update_marker_data>(), 8);
        // _IOWR('F', 0x2F, 8 bytes)
        let expected = (3u32 << 30) | (8 << 16) | (0x46 << 8) | 0x2F;
        assert_eq!(expected, HWTCON_WAIT_FOR_UPDATE_COMPLETE);
    }

    #[test]
    fn alt_buffer_layouts_differ_by_virt_addr() {
        // NTX adds virt_addr; the exact +4 relationship holds on the
        // device's 32-bit ABI (64-bit hosts add alignment padding).
        assert!(
            std::mem::size_of::<mxcfb_alt_buffer_data_ntx>()
                > std::mem::size_of::<mxcfb_alt_buffer_data>()
        );
        #[cfg(target_pointer_width = "32")]
        assert_eq!(
            std::mem::size_of::<mxcfb_alt_buffer_data_ntx>(),
            std::mem::size_of::<mxcfb_alt_buffer_data>() + 4
        );
    }
}
