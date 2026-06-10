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

use gideon_render::GrayPage;
use memmap2::MmapMut;

use crate::{blit_into, Display, Error, RefreshMode, Result};

// --- Linux fb + mxcfb ABI (from linux/fb.h and mxcfb.h) ---

const FBIOGET_VSCREENINFO: u32 = 0x4600;
const FBIOGET_FSCREENINFO: u32 = 0x4602;
// _IOW('F', 0x2E, struct mxcfb_update_data_v1) — pre-Mark 7 kernels.
const MXCFB_SEND_UPDATE_V1: u32 = 0x4048462E;
// _IOW('F', 0x2E, struct mxcfb_update_data_v2) — Mark 7+ (Clara HD and
// newer): same request, but the struct gained dither_mode/quant_bit, so
// the encoded size (and therefore the ioctl number) differs.
const MXCFB_SEND_UPDATE_V2: u32 = 0x4050462E;
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

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct mxcfb_alt_buffer_data {
    virt_addr: libc::c_ulong,
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
    alt_buffer_data: mxcfb_alt_buffer_data,
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

/// Which MXCFB_SEND_UPDATE generation the kernel speaks; probed on the
/// first refresh and cached.
#[derive(Clone, Copy, PartialEq)]
enum UpdateApi {
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
        })
    }
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
        Ok(())
    }

    fn flush(&mut self, mode: RefreshMode) -> Result<()> {
        self.map.flush()?;

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
            Some(UpdateApi::V2) => &[UpdateApi::V2],
            Some(UpdateApi::V1) => &[UpdateApi::V1],
            None => &[UpdateApi::V2, UpdateApi::V1],
        };

        let mut last_err = None;
        for &api in candidates {
            let ret = match api {
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
                return Ok(());
            }
            last_err = Some(std::io::Error::last_os_error());
        }
        Err(Error::Display(format!(
            "MXCFB_SEND_UPDATE rejected (tried v2+v1, {}x{}, mode={update_mode}): {}",
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

#[cfg(test)]
mod pixel_format_tests {
    use super::write_gray_row;

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
        // The structs contain c_ulong, so their size matches the device
        // (32-bit ARM) layout only there; the constants are fixed to the
        // device ABI: 0x48 and 0x50 bytes.
        assert_eq!(iow_f_2e(0x48), MXCFB_SEND_UPDATE_V1);
        assert_eq!(iow_f_2e(0x50), MXCFB_SEND_UPDATE_V2);
        #[cfg(target_pointer_width = "32")]
        {
            assert_eq!(std::mem::size_of::<mxcfb_update_data_v1>(), 0x48);
            assert_eq!(std::mem::size_of::<mxcfb_update_data_v2>(), 0x50);
        }
    }

    #[test]
    fn v2_is_v1_plus_dither_fields() {
        assert_eq!(
            std::mem::size_of::<mxcfb_update_data_v2>(),
            std::mem::size_of::<mxcfb_update_data_v1>() + 8
        );
    }
}
