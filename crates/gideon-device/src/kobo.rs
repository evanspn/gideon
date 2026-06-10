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
// _IOW('F', 0x2E, struct mxcfb_update_data)
const MXCFB_SEND_UPDATE: u32 = 0x4048462E;

/// `libc::ioctl`'s request parameter is `c_ulong` on glibc but `c_int` on
/// 32-bit musl (the Kobo target); this wrapper papers over the difference.
///
/// # Safety
/// Same contract as `libc::ioctl`: `arg` must match what `request` expects.
unsafe fn ioctl<T>(fd: libc::c_int, request: u32, arg: *mut T) -> libc::c_int {
    libc::ioctl(fd, request as _, arg)
}

const WAVEFORM_MODE_AUTO: u32 = 0x101;
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
struct mxcfb_update_data {
    update_region: mxcfb_rect,
    waveform_mode: u32,
    update_mode: u32,
    update_marker: u32,
    temp: i32,
    flags: u32,
    alt_buffer_data: mxcfb_alt_buffer_data,
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
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
            if ioctl(file.as_raw_fd(), FBIOGET_FSCREENINFO, &mut fix) != 0 {
                return Err(Error::Io(std::io::Error::last_os_error()));
            }
        }

        // Stock Nickel leaves the panel at 16 or 32bpp; we convert grayscale
        // to whatever depth the framebuffer reports instead of requiring a
        // depth switch.
        if !matches!(var.bits_per_pixel, 8 | 16 | 24 | 32) {
            return Err(Error::Display(format!(
                "unsupported framebuffer depth: {}bpp (supported: 8/16/24/32)",
                var.bits_per_pixel
            )));
        }

        // SAFETY: mapping the framebuffer memory the kernel advertised.
        let map = unsafe { MmapMut::map_mut(&file)? };

        Ok(Self {
            file,
            map,
            width: var.xres,
            height: var.yres,
            line_length: fix.line_length,
            bytes_per_pixel: var.bits_per_pixel / 8,
            update_marker: 0,
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
        let update = mxcfb_update_data {
            update_region: mxcfb_rect {
                top: 0,
                left: 0,
                width: self.width,
                height: self.height,
            },
            waveform_mode: WAVEFORM_MODE_AUTO,
            update_mode: match mode {
                RefreshMode::Full => UPDATE_MODE_FULL,
                RefreshMode::Partial => UPDATE_MODE_PARTIAL,
            },
            update_marker: self.update_marker,
            temp: TEMP_USE_AMBIENT,
            ..Default::default()
        };

        // SAFETY: MXCFB_SEND_UPDATE with a fully initialized update struct.
        let mut update = update;
        let ret = unsafe { ioctl(self.file.as_raw_fd(), MXCFB_SEND_UPDATE, &mut update) };
        if ret != 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(())
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
