//! gideon-render: turns decoded page images into framebuffer-ready
//! grayscale pixels for e-ink displays.
//!
//! Pipeline: decode (done by gideon-core) → scale to fit the screen →
//! grayscale → optional Floyd–Steinberg dithering down to 16 gray levels
//! (what most Kobo e-ink panels can actually show) → centered composite
//! onto a white canvas matching the screen size.

pub mod shelf;
pub mod text;

use image::imageops::FilterType;
use image::DynamicImage;

/// How a page is fitted to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FitMode {
    /// Scale so the whole page is visible (letterboxed). The default.
    #[default]
    Contain,
    /// Scale to fill the screen width (page may scroll vertically).
    FitWidth,
    /// Scale to fill the screen height.
    FitHeight,
}

impl FitMode {
    /// Parse a settings string leniently: "fit-width" selects
    /// [`FitMode::FitWidth`]; anything else (including unknown values)
    /// means [`FitMode::Contain`].
    pub fn from_setting(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "fit-width" => FitMode::FitWidth,
            _ => FitMode::Contain,
        }
    }
}

/// A rendered page: 8-bit grayscale pixels, row-major, `width * height` long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrayPage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl GrayPage {
    pub fn new_white(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0xFF; (width * height) as usize],
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> u8 {
        self.pixels[(y * self.width + x) as usize]
    }
}

/// A rendered color page: 8-bit RGB pixels, row-major, 3 bytes per pixel
/// (`width * height * 3` long). Used where color survives to the panel —
/// today the library shelf's covers on Kaleido devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RgbPage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl RgbPage {
    pub fn new_white(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0xFF; (width * height * 3) as usize],
        }
    }

    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        let idx = ((y * self.width + x) * 3) as usize;
        [self.pixels[idx], self.pixels[idx + 1], self.pixels[idx + 2]]
    }

    /// Replicate a grayscale page into RGB (equal channels).
    pub fn from_gray(gray: &GrayPage) -> Self {
        let mut pixels = Vec::with_capacity(gray.pixels.len() * 3);
        for &g in &gray.pixels {
            pixels.extend_from_slice(&[g, g, g]);
        }
        Self {
            width: gray.width,
            height: gray.height,
            pixels,
        }
    }

    /// Collapse to grayscale with Rec.601 luma — what grayscale panels
    /// (and the default device blit) show for color content.
    pub fn to_gray(&self) -> GrayPage {
        GrayPage {
            width: self.width,
            height: self.height,
            pixels: self
                .pixels
                .chunks_exact(3)
                .map(|px| luma_rec601(px[0], px[1], px[2]))
                .collect(),
        }
    }
}

/// Rec.601 luma: 0.299 R + 0.587 G + 0.114 B, rounded.
pub fn luma_rec601(r: u8, g: u8, b: u8) -> u8 {
    ((299 * r as u32 + 587 * g as u32 + 114 * b as u32 + 500) / 1000) as u8
}

/// A sample counts as colorful when its channel spread exceeds this.
/// JPEG chroma noise on grayscale scans stays well below 20; real color
/// art (skin tones, hair, spot color) is far above it.
const COLOR_SPREAD_THRESHOLD: u8 = 20;

/// Decide whether a decoded page holds real COLOR content (and should take
/// the Kaleido RGB path) or is grayscale manga.
///
/// Pure and cheap: samples every 8th pixel in both directions (1/64th of
/// the page). A sample is colorful when `max(|r-g|, |g-b|, |r-b|) > 20`,
/// and the page is color when **more than 1%** of samples are colorful —
/// so sepia-ish scan noise or a single tinted border can't drag a B/W
/// page off the fast grayscale path.
pub fn page_is_color(page: &DynamicImage) -> bool {
    use image::GenericImageView;

    // Buffers without color channels can't be color, whatever they hold.
    if !page.color().has_color() {
        return false;
    }
    let mut samples: u64 = 0;
    let mut colorful: u64 = 0;
    for y in (0..page.height()).step_by(8) {
        for x in (0..page.width()).step_by(8) {
            let [r, g, b, _] = page.get_pixel(x, y).0;
            let spread = r.abs_diff(g).max(g.abs_diff(b)).max(r.abs_diff(b));
            samples += 1;
            if spread > COLOR_SPREAD_THRESHOLD {
                colorful += 1;
            }
        }
    }
    // "> 1%" exactly: colorful / samples > 1/100.
    colorful * 100 > samples
}

/// Rendering options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    pub screen_width: u32,
    pub screen_height: u32,
    pub fit: FitMode,
    /// Dither to 16 gray levels for e-ink panels. Disable for desktop preview.
    pub dither: bool,
}

impl RenderOptions {
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            screen_width,
            screen_height,
            fit: FitMode::default(),
            dither: true,
        }
    }
}

/// Compute the scaled size of a `src_w x src_h` page under `fit` for the
/// given screen, preserving aspect ratio. Never returns zero dimensions.
pub fn compute_fit(
    src_w: u32,
    src_h: u32,
    screen_w: u32,
    screen_h: u32,
    fit: FitMode,
) -> (u32, u32) {
    let (src_w, src_h) = (src_w.max(1) as f64, src_h.max(1) as f64);
    let (screen_w_f, screen_h_f) = (screen_w as f64, screen_h as f64);

    let scale = match fit {
        FitMode::Contain => (screen_w_f / src_w).min(screen_h_f / src_h),
        FitMode::FitWidth => screen_w_f / src_w,
        FitMode::FitHeight => screen_h_f / src_h,
    };

    let w = (src_w * scale).round().max(1.0) as u32;
    let h = (src_h * scale).round().max(1.0) as u32;
    (w, h)
}

/// A rendered page, in whichever depth the source content demanded:
/// grayscale for B/W manga (the overwhelmingly common case — small,
/// dithered, fast) or RGB for pages with real color (Kaleido panels show
/// it; everyone else collapses it to luma at blit time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageBuf {
    Gray(GrayPage),
    Rgb(RgbPage),
}

impl PageBuf {
    pub fn width(&self) -> u32 {
        match self {
            PageBuf::Gray(page) => page.width,
            PageBuf::Rgb(page) => page.width,
        }
    }

    pub fn height(&self) -> u32 {
        match self {
            PageBuf::Gray(page) => page.height,
            PageBuf::Rgb(page) => page.height,
        }
    }

    /// `width * height` regardless of variant. Cache budgets must compare
    /// PIXEL counts, never byte lengths — an RGB page is 3x the bytes of
    /// gray at equal pixels and must not look three times "bigger".
    pub fn pixel_count(&self) -> usize {
        self.width() as usize * self.height() as usize
    }

    pub fn is_color(&self) -> bool {
        matches!(self, PageBuf::Rgb(_))
    }

    /// Collapse to grayscale: identity for gray pages, Rec.601 luma for
    /// color ones.
    pub fn into_gray(self) -> GrayPage {
        match self {
            PageBuf::Gray(page) => page,
            PageBuf::Rgb(page) => page.to_gray(),
        }
    }

    /// [`crop_rows`] / [`crop_rows_rgb`], keeping the variant.
    pub fn crop_rows(&self, offset_y: u32, height: u32) -> PageBuf {
        match self {
            PageBuf::Gray(page) => PageBuf::Gray(crop_rows(page, offset_y, height)),
            PageBuf::Rgb(page) => PageBuf::Rgb(crop_rows_rgb(page, offset_y, height)),
        }
    }

    /// [`rotate_page`] / [`rotate_page_rgb`], keeping the variant.
    pub fn rotate(&self, degrees: u32) -> PageBuf {
        match self {
            PageBuf::Gray(page) => PageBuf::Gray(rotate_page(page, degrees)),
            PageBuf::Rgb(page) => PageBuf::Rgb(rotate_page_rgb(page, degrees)),
        }
    }
}

/// Render a page image to a screen-sized canvas: grayscale (dithered per
/// `opts`) for B/W pages, RGB for pages [`page_is_color`] detects as color.
///
/// For [`FitMode::Contain`] the page is centered with white margins. For the
/// other modes the canvas grows beyond the screen in one dimension; the
/// caller is responsible for scrolling/cropping when blitting.
///
/// Color pages keep their RGB samples through the resize (same filter
/// choice as gray) and are NEVER software-dithered: the Kaleido refresh
/// path dithers in hardware (HWTCON Y8→Y4), and pre-quantizing each
/// channel would just band the color.
pub fn render_page(page: &DynamicImage, opts: &RenderOptions) -> PageBuf {
    let (target_w, target_h) = compute_fit(
        page.width(),
        page.height(),
        opts.screen_width,
        opts.screen_height,
        opts.fit,
    );

    // Downscales (the normal case) use Triangle: ~4x faster than Lanczos3
    // on the device's ARM core and indistinguishable for manga once
    // dithered. Upscales (low-res sources) use CatmullRom — Triangle is
    // visibly soft when enlarging.
    let filter = if target_w > page.width() || target_h > page.height() {
        FilterType::CatmullRom
    } else {
        FilterType::Triangle
    };

    let canvas_w = opts.screen_width.max(target_w);
    let canvas_h = opts.screen_height.max(target_h);
    let off_x = (canvas_w - target_w) / 2;
    let off_y = (canvas_h - target_h) / 2;

    if page_is_color(page) {
        let scaled = page.resize_exact(target_w, target_h, filter).into_rgb8();
        let mut canvas = RgbPage::new_white(canvas_w, canvas_h);
        let src = scaled.as_raw();
        for y in 0..target_h {
            let dst_start = (((y + off_y) * canvas_w + off_x) * 3) as usize;
            let src_start = (y * target_w * 3) as usize;
            let row_len = (target_w * 3) as usize;
            canvas.pixels[dst_start..dst_start + row_len]
                .copy_from_slice(&src[src_start..src_start + row_len]);
        }
        return PageBuf::Rgb(canvas);
    }

    let scaled = page.resize_exact(target_w, target_h, filter).into_luma8();
    let mut canvas = GrayPage::new_white(canvas_w, canvas_h);

    for y in 0..target_h {
        let canvas_row = ((y + off_y) * canvas_w + off_x) as usize;
        for x in 0..target_w {
            canvas.pixels[canvas_row + x as usize] = scaled.get_pixel(x, y).0[0];
        }
    }

    if opts.dither {
        dither_to_16_levels(&mut canvas);
    }

    PageBuf::Gray(canvas)
}

/// Rotate a rendered page clockwise by `degrees` (0, 90, 180 or 270).
///
/// 0 — and any value that isn't a multiple of 90 — returns the page
/// unchanged. 90 and 270 swap the page's width and height.
pub fn rotate_page(page: &GrayPage, degrees: u32) -> GrayPage {
    let (w, h) = (page.width, page.height);
    match degrees % 360 {
        // Clockwise 90: (x, y) → (h - 1 - y, x).
        90 => {
            let mut out = GrayPage::new_white(h, w);
            for y in 0..h {
                for x in 0..w {
                    out.pixels[(x * h + (h - 1 - y)) as usize] = page.pixel(x, y);
                }
            }
            out
        }
        // 180: (x, y) → (w - 1 - x, h - 1 - y).
        180 => {
            let mut out = GrayPage::new_white(w, h);
            for y in 0..h {
                for x in 0..w {
                    out.pixels[((h - 1 - y) * w + (w - 1 - x)) as usize] = page.pixel(x, y);
                }
            }
            out
        }
        // Clockwise 270 (= counter-clockwise 90): (x, y) → (y, w - 1 - x).
        270 => {
            let mut out = GrayPage::new_white(h, w);
            for y in 0..h {
                for x in 0..w {
                    out.pixels[((w - 1 - x) * h + y) as usize] = page.pixel(x, y);
                }
            }
            out
        }
        _ => page.clone(),
    }
}

/// Rotate a rendered COLOR page clockwise by `degrees` (0, 90, 180 or 270).
///
/// Same coordinate mapping as [`rotate_page`], with x3 index math for the
/// packed RGB triples.
pub fn rotate_page_rgb(page: &RgbPage, degrees: u32) -> RgbPage {
    let (w, h) = (page.width, page.height);
    let copy_px = |out: &mut RgbPage, dst_idx: u32, x: u32, y: u32| {
        let src = ((y * w + x) * 3) as usize;
        let dst = (dst_idx * 3) as usize;
        out.pixels[dst..dst + 3].copy_from_slice(&page.pixels[src..src + 3]);
    };
    match degrees % 360 {
        // Clockwise 90: (x, y) → (h - 1 - y, x).
        90 => {
            let mut out = RgbPage::new_white(h, w);
            for y in 0..h {
                for x in 0..w {
                    copy_px(&mut out, x * h + (h - 1 - y), x, y);
                }
            }
            out
        }
        // 180: (x, y) → (w - 1 - x, h - 1 - y).
        180 => {
            let mut out = RgbPage::new_white(w, h);
            for y in 0..h {
                for x in 0..w {
                    copy_px(&mut out, (h - 1 - y) * w + (w - 1 - x), x, y);
                }
            }
            out
        }
        // Clockwise 270 (= counter-clockwise 90): (x, y) → (y, w - 1 - x).
        270 => {
            let mut out = RgbPage::new_white(h, w);
            for y in 0..h {
                for x in 0..w {
                    copy_px(&mut out, (w - 1 - x) * h + y, x, y);
                }
            }
            out
        }
        _ => page.clone(),
    }
}

/// Copy `height` rows of `page` starting at `offset_y` (both clamped to
/// the page) — the reader's vertical scroll window.
pub fn crop_rows(page: &GrayPage, offset_y: u32, height: u32) -> GrayPage {
    let offset_y = offset_y.min(page.height.saturating_sub(1));
    let height = height.min(page.height - offset_y);
    let start = (offset_y * page.width) as usize;
    let end = start + (height * page.width) as usize;
    GrayPage {
        width: page.width,
        height,
        pixels: page.pixels[start..end].to_vec(),
    }
}

/// [`crop_rows`] for COLOR pages: same clamping, x3 index math.
pub fn crop_rows_rgb(page: &RgbPage, offset_y: u32, height: u32) -> RgbPage {
    let offset_y = offset_y.min(page.height.saturating_sub(1));
    let height = height.min(page.height - offset_y);
    let start = (offset_y * page.width * 3) as usize;
    let end = start + (height * page.width * 3) as usize;
    RgbPage {
        width: page.width,
        height,
        pixels: page.pixels[start..end].to_vec(),
    }
}

/// In-place Floyd–Steinberg dithering down to 16 evenly spaced gray levels.
pub fn dither_to_16_levels(page: &mut GrayPage) {
    let w = page.width as usize;
    let h = page.height as usize;
    // Error diffusion buffer in higher precision.
    let mut buf: Vec<i32> = page.pixels.iter().map(|&p| p as i32).collect();

    for y in 0..h {
        for x in 0..w {
            let idx = y * w + x;
            let old = buf[idx].clamp(0, 255);
            let new = quantize_16(old as u8) as i32;
            page.pixels[idx] = new as u8;
            let err = old - new;

            if x + 1 < w {
                buf[idx + 1] += err * 7 / 16;
            }
            if y + 1 < h {
                if x > 0 {
                    buf[idx + w - 1] += err * 3 / 16;
                }
                buf[idx + w] += err * 5 / 16;
                if x + 1 < w {
                    buf[idx + w + 1] += err / 16;
                }
            }
        }
    }
}

/// Snap an 8-bit gray value to the nearest of 16 evenly spaced levels
/// (0x00, 0x11, 0x22, … 0xFF) — matching 4-bit e-ink panel depth.
pub fn quantize_16(value: u8) -> u8 {
    let level = (value as u16 + 8) / 17;
    (level.min(15) * 17) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn solid_page(w: u32, h: u32, gray: u8) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([gray, gray, gray])))
    }

    /// Render a page the tests know is B/W and unwrap the gray buffer.
    fn render_gray(page: &DynamicImage, opts: &RenderOptions) -> GrayPage {
        match render_page(page, opts) {
            PageBuf::Gray(page) => page,
            PageBuf::Rgb(_) => panic!("B/W page took the RGB path"),
        }
    }

    #[test]
    fn contain_letterboxes_tall_screen() {
        // 1000x1000 page on a 600x800 screen → 600x600.
        assert_eq!(
            compute_fit(1000, 1000, 600, 800, FitMode::Contain),
            (600, 600)
        );
    }

    #[test]
    fn contain_letterboxes_wide_page() {
        // 2000x1000 page on a 600x800 screen → fit by width.
        assert_eq!(
            compute_fit(2000, 1000, 600, 800, FitMode::Contain),
            (600, 300)
        );
    }

    #[test]
    fn fit_width_overflows_height() {
        let (w, h) = compute_fit(1000, 3000, 600, 800, FitMode::FitWidth);
        assert_eq!(w, 600);
        assert_eq!(h, 1800);
    }

    #[test]
    fn fit_height_overflows_width() {
        let (w, h) = compute_fit(3000, 1000, 600, 800, FitMode::FitHeight);
        assert_eq!(h, 800);
        assert_eq!(w, 2400);
    }

    #[test]
    fn fit_never_returns_zero() {
        let (w, h) = compute_fit(10000, 1, 600, 800, FitMode::Contain);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn render_contain_centers_with_white_margins() {
        let page = solid_page(100, 100, 0); // black square
        let opts = RenderOptions {
            screen_width: 200,
            screen_height: 100,
            fit: FitMode::Contain,
            dither: false,
        };
        let out = render_gray(&page, &opts);
        assert_eq!((out.width, out.height), (200, 100));
        // Margins are white, center is black.
        assert_eq!(out.pixel(0, 50), 0xFF);
        assert_eq!(out.pixel(199, 50), 0xFF);
        assert_eq!(out.pixel(100, 50), 0x00);
    }

    #[test]
    fn render_fit_width_canvas_grows_vertically() {
        let page = solid_page(100, 400, 128);
        let opts = RenderOptions {
            screen_width: 200,
            screen_height: 100,
            fit: FitMode::FitWidth,
            dither: false,
        };
        let out = render_gray(&page, &opts);
        assert_eq!(out.width, 200);
        assert_eq!(out.height, 800);
    }

    /// Parity pin: a B/W page through [`render_page`] must be
    /// byte-identical to the historical grayscale-only pipeline
    /// (resize → luma → centered white canvas → Floyd–Steinberg to 16
    /// levels), replicated inline here. If this breaks, the gray fast
    /// path changed — that is never an intended side effect of color work.
    #[test]
    fn gray_render_is_byte_identical_to_the_legacy_pipeline() {
        // A gradient exercises resize filtering and error diffusion.
        let mut img = RgbImage::new(96, 128);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let g = ((x * 2 + y) % 256) as u8;
            *px = image::Rgb([g, g, g]);
        }
        let page = DynamicImage::ImageRgb8(img);
        let opts = RenderOptions {
            screen_width: 64,
            screen_height: 64,
            fit: FitMode::Contain,
            dither: true,
        };

        // The legacy pipeline, verbatim.
        let (target_w, target_h) = compute_fit(96, 128, 64, 64, FitMode::Contain);
        let filter = if target_w > 96 || target_h > 128 {
            FilterType::CatmullRom
        } else {
            FilterType::Triangle
        };
        let scaled = page.resize_exact(target_w, target_h, filter).into_luma8();
        let canvas_w = 64.max(target_w);
        let canvas_h = 64.max(target_h);
        let mut expected = GrayPage::new_white(canvas_w, canvas_h);
        let off_x = (canvas_w - target_w) / 2;
        let off_y = (canvas_h - target_h) / 2;
        for y in 0..target_h {
            let row = ((y + off_y) * canvas_w + off_x) as usize;
            for x in 0..target_w {
                expected.pixels[row + x as usize] = scaled.get_pixel(x, y).0[0];
            }
        }
        dither_to_16_levels(&mut expected);

        assert_eq!(render_page(&page, &opts), PageBuf::Gray(expected));
    }

    #[test]
    fn color_page_renders_rgb_centered_and_undithered() {
        // A solid color page on a wider screen: centered, white margins,
        // color preserved exactly (solid input → resize is lossless).
        let red = image::Rgb([200, 30, 30]);
        let page = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 100, red));
        let opts = RenderOptions {
            screen_width: 200,
            screen_height: 100,
            fit: FitMode::Contain,
            dither: true, // requests gray dithering — color must SKIP it
        };
        let PageBuf::Rgb(out) = render_page(&page, &opts) else {
            panic!("color page took the gray path");
        };
        assert_eq!((out.width, out.height), (200, 100));
        // Margins are white, center keeps the exact color: 200 and 30 are
        // not 17-multiples, so any sneaky quantization would change them.
        assert_eq!(out.pixel(0, 50), [0xFF, 0xFF, 0xFF]);
        assert_eq!(out.pixel(199, 50), [0xFF, 0xFF, 0xFF]);
        assert_eq!(out.pixel(100, 50), [200, 30, 30]);
    }

    #[test]
    fn color_fit_width_canvas_grows_vertically() {
        let blue = image::Rgb([40, 60, 220]);
        let page = DynamicImage::ImageRgb8(RgbImage::from_pixel(100, 400, blue));
        let opts = RenderOptions {
            screen_width: 200,
            screen_height: 100,
            fit: FitMode::FitWidth,
            dither: true,
        };
        let out = render_page(&page, &opts);
        assert!(out.is_color());
        assert_eq!((out.width(), out.height()), (200, 800));
        assert_eq!(out.pixel_count(), 200 * 800);
    }

    #[test]
    fn page_buf_into_gray_collapses_color() {
        let red = image::Rgb([255, 0, 0]);
        let page = DynamicImage::ImageRgb8(RgbImage::from_pixel(10, 10, red));
        let opts = RenderOptions {
            screen_width: 10,
            screen_height: 10,
            fit: FitMode::Contain,
            dither: false,
        };
        let gray = render_page(&page, &opts).into_gray();
        assert_eq!(gray.pixel(5, 5), 76, "Rec.601 red luma");
    }

    /// A 2x3 page with every pixel distinct, so rotations are fully
    /// position-sensitive:
    ///
    /// ```text
    /// 0 1
    /// 2 3
    /// 4 5
    /// ```
    fn asymmetric_page() -> GrayPage {
        GrayPage {
            width: 2,
            height: 3,
            pixels: vec![0, 1, 2, 3, 4, 5],
        }
    }

    #[test]
    fn rotate_0_is_identity() {
        let page = asymmetric_page();
        assert_eq!(rotate_page(&page, 0), page);
        // 360 wraps to 0; non-multiples of 90 are treated as 0 too.
        assert_eq!(rotate_page(&page, 360), page);
        assert_eq!(rotate_page(&page, 45), page);
    }

    #[test]
    fn rotate_90_clockwise_exact_pixels() {
        let out = rotate_page(&asymmetric_page(), 90);
        assert_eq!((out.width, out.height), (3, 2));
        // Top row of the source becomes the right column.
        assert_eq!(out.pixels, vec![4, 2, 0, 5, 3, 1]);
        assert_eq!(out.pixel(2, 0), 0);
        assert_eq!(out.pixel(2, 1), 1);
        assert_eq!(out.pixel(0, 0), 4);
    }

    #[test]
    fn rotate_180_exact_pixels() {
        let out = rotate_page(&asymmetric_page(), 180);
        assert_eq!((out.width, out.height), (2, 3));
        assert_eq!(out.pixels, vec![5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn rotate_270_clockwise_exact_pixels() {
        let out = rotate_page(&asymmetric_page(), 270);
        assert_eq!((out.width, out.height), (3, 2));
        // Top row of the source becomes the left column.
        assert_eq!(out.pixels, vec![1, 3, 5, 0, 2, 4]);
        assert_eq!(out.pixel(0, 1), 0);
        assert_eq!(out.pixel(0, 0), 1);
        assert_eq!(out.pixel(2, 0), 5);
    }

    #[test]
    fn rotations_compose_back_to_identity() {
        let page = asymmetric_page();
        let through_90s = rotate_page(
            &rotate_page(&rotate_page(&rotate_page(&page, 90), 90), 90),
            90,
        );
        assert_eq!(through_90s, page);
        assert_eq!(rotate_page(&rotate_page(&page, 90), 270), page);
        assert_eq!(rotate_page(&rotate_page(&page, 180), 180), page);
    }

    // --- RGB rotation & crop (mirrors the gray pixel tests) ---

    /// The RGB twin of [`asymmetric_page`]: a 2x3 page whose pixel at
    /// gray-index `i` is the triple `[i, 10+i, 20+i]` — every channel of
    /// every pixel distinct, so rotations are fully position- and
    /// channel-sensitive.
    fn asymmetric_page_rgb() -> RgbPage {
        let mut pixels = Vec::new();
        for i in 0..6u8 {
            pixels.extend_from_slice(&[i, 10 + i, 20 + i]);
        }
        RgbPage {
            width: 2,
            height: 3,
            pixels,
        }
    }

    /// The pixel triples of `page` in row-major order, reduced to their
    /// gray-index (first channel) for easy comparison with the gray tests.
    fn rgb_indices(page: &RgbPage) -> Vec<u8> {
        page.pixels.chunks_exact(3).map(|px| px[0]).collect()
    }

    #[test]
    fn rotate_rgb_0_is_identity() {
        let page = asymmetric_page_rgb();
        assert_eq!(rotate_page_rgb(&page, 0), page);
        assert_eq!(rotate_page_rgb(&page, 360), page);
        assert_eq!(rotate_page_rgb(&page, 45), page);
    }

    #[test]
    fn rotate_rgb_90_clockwise_exact_pixels() {
        let out = rotate_page_rgb(&asymmetric_page_rgb(), 90);
        assert_eq!((out.width, out.height), (3, 2));
        // Same ordering as the gray test…
        assert_eq!(rgb_indices(&out), vec![4, 2, 0, 5, 3, 1]);
        // …and the triples travel whole: channels never shear apart.
        assert_eq!(out.pixel(2, 0), [0, 10, 20]);
        assert_eq!(out.pixel(2, 1), [1, 11, 21]);
        assert_eq!(out.pixel(0, 0), [4, 14, 24]);
    }

    #[test]
    fn rotate_rgb_180_exact_pixels() {
        let out = rotate_page_rgb(&asymmetric_page_rgb(), 180);
        assert_eq!((out.width, out.height), (2, 3));
        assert_eq!(rgb_indices(&out), vec![5, 4, 3, 2, 1, 0]);
        assert_eq!(out.pixel(0, 0), [5, 15, 25]);
    }

    #[test]
    fn rotate_rgb_270_clockwise_exact_pixels() {
        let out = rotate_page_rgb(&asymmetric_page_rgb(), 270);
        assert_eq!((out.width, out.height), (3, 2));
        assert_eq!(rgb_indices(&out), vec![1, 3, 5, 0, 2, 4]);
        assert_eq!(out.pixel(0, 1), [0, 10, 20]);
        assert_eq!(out.pixel(0, 0), [1, 11, 21]);
        assert_eq!(out.pixel(2, 0), [5, 15, 25]);
    }

    #[test]
    fn rgb_rotations_compose_back_to_identity() {
        let page = asymmetric_page_rgb();
        let through_90s = rotate_page_rgb(
            &rotate_page_rgb(&rotate_page_rgb(&rotate_page_rgb(&page, 90), 90), 90),
            90,
        );
        assert_eq!(through_90s, page);
        assert_eq!(rotate_page_rgb(&rotate_page_rgb(&page, 90), 270), page);
        assert_eq!(rotate_page_rgb(&rotate_page_rgb(&page, 180), 180), page);
    }

    #[test]
    fn rgb_rotation_matches_gray_rotation_through_luma() {
        // Rotating then collapsing must equal collapsing then rotating:
        // the two index paths can never disagree.
        let page = asymmetric_page_rgb();
        for degrees in [0, 90, 180, 270] {
            assert_eq!(
                rotate_page_rgb(&page, degrees).to_gray(),
                rotate_page(&page.to_gray(), degrees),
                "mismatch at {degrees}°"
            );
        }
    }

    #[test]
    fn crop_rows_extracts_the_window_gray_and_rgb() {
        let page = GrayPage {
            width: 2,
            height: 4,
            pixels: vec![0, 0, 1, 1, 2, 2, 3, 3],
        };
        let window = crop_rows(&page, 1, 2);
        assert_eq!((window.width, window.height), (2, 2));
        assert_eq!(window.pixels, vec![1, 1, 2, 2]);
        // Clamped at the bottom.
        assert_eq!(crop_rows(&page, 3, 5).pixels, vec![3, 3]);

        // The RGB crop must select exactly the same rows.
        let rgb = RgbPage::from_gray(&page);
        let rgb_window = crop_rows_rgb(&rgb, 1, 2);
        assert_eq!((rgb_window.width, rgb_window.height), (2, 2));
        assert_eq!(rgb_window, RgbPage::from_gray(&window));
        assert_eq!(
            crop_rows_rgb(&rgb, 3, 5).pixels,
            vec![3, 3, 3, 3, 3, 3],
            "clamped RGB tail"
        );
    }

    #[test]
    fn page_buf_crop_and_rotate_keep_the_variant() {
        let gray = PageBuf::Gray(asymmetric_page());
        let rgb = PageBuf::Rgb(asymmetric_page_rgb());
        assert!(!gray.crop_rows(1, 2).is_color());
        assert!(rgb.crop_rows(1, 2).is_color());
        assert!(!gray.rotate(90).is_color());
        assert!(rgb.rotate(90).is_color());
        // And dispatch to the exact same math as the direct calls.
        assert_eq!(
            rgb.rotate(90),
            PageBuf::Rgb(rotate_page_rgb(&asymmetric_page_rgb(), 90))
        );
        assert_eq!(
            gray.crop_rows(1, 2),
            PageBuf::Gray(crop_rows(&asymmetric_page(), 1, 2))
        );
    }

    #[test]
    fn fit_mode_setting_parses_leniently() {
        assert_eq!(FitMode::from_setting("fit-width"), FitMode::FitWidth);
        assert_eq!(FitMode::from_setting("  Fit-Width "), FitMode::FitWidth);
        assert_eq!(FitMode::from_setting("contain"), FitMode::Contain);
        assert_eq!(FitMode::from_setting("sideways"), FitMode::Contain);
        assert_eq!(FitMode::from_setting(""), FitMode::Contain);
    }

    #[test]
    fn rgb_page_round_trips_gray_and_collapses_with_rec601() {
        let gray = GrayPage {
            width: 2,
            height: 1,
            pixels: vec![0x12, 0xAB],
        };
        let rgb = RgbPage::from_gray(&gray);
        assert_eq!(rgb.pixel(0, 0), [0x12, 0x12, 0x12]);
        // Gray in, gray out: equal channels survive the luma exactly.
        assert_eq!(rgb.to_gray(), gray);

        // Primaries collapse with the Rec.601 weights.
        let color = RgbPage {
            width: 3,
            height: 1,
            pixels: vec![255, 0, 0, 0, 255, 0, 0, 0, 255],
        };
        assert_eq!(color.to_gray().pixels, vec![76, 150, 29]);
    }

    // --- color detection ---

    #[test]
    fn gray_page_is_not_color() {
        // Equal channels everywhere: zero spread, zero colorful samples.
        assert!(!page_is_color(&solid_page(64, 64, 0x80)));
        // A grayscale buffer (no color channels) is never color.
        let luma =
            DynamicImage::ImageLuma8(image::GrayImage::from_pixel(64, 64, image::Luma([0x80])));
        assert!(!page_is_color(&luma));
    }

    #[test]
    fn color_page_is_detected() {
        // A solid red page: every sample has spread 255.
        let red = DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 64, image::Rgb([200, 30, 30])));
        assert!(page_is_color(&red));
        // A page that's mostly gray but with a real color panel (top
        // quarter) is still a color page — way past the 1% of samples.
        let mut img = RgbImage::from_pixel(64, 64, image::Rgb([0x80, 0x80, 0x80]));
        for y in 0..16 {
            for x in 0..64 {
                img.put_pixel(x, y, image::Rgb([220, 60, 40]));
            }
        }
        assert!(page_is_color(&DynamicImage::ImageRgb8(img)));
    }

    #[test]
    fn tinted_noise_stays_grayscale() {
        // JPEG-style chroma noise: small per-pixel channel spreads (≤ 20)
        // on a gray scan must NOT count as color, however widespread.
        let mut img = RgbImage::new(64, 64);
        for (x, y, px) in img.enumerate_pixels_mut() {
            let g = 0x80u8;
            let wobble = ((x + y) % 21) as u8; // spreads 0..=20, never > 20
            *px = image::Rgb([g.saturating_add(wobble), g, g]);
        }
        assert!(!page_is_color(&DynamicImage::ImageRgb8(img)));

        // And a tiny tinted region (a single sampled pixel out of 64 = ~1.5%
        // would flip it, so keep it at exactly 1 of 100+ samples → ≤ 1%):
        // an 80x80 page samples a 10x10 grid; one strong-color sample is
        // exactly 1% — NOT more than 1% — and stays grayscale.
        let mut img = RgbImage::from_pixel(80, 80, image::Rgb([0x80, 0x80, 0x80]));
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        assert!(!page_is_color(&DynamicImage::ImageRgb8(img)));
    }

    #[test]
    fn quantize_snaps_to_16_levels() {
        assert_eq!(quantize_16(0), 0);
        assert_eq!(quantize_16(255), 255);
        assert_eq!(quantize_16(8), 0);
        assert_eq!(quantize_16(9), 17);
        for v in 0..=255u16 {
            let q = quantize_16(v as u8);
            assert_eq!(q % 17, 0, "quantized value {q} is not a 17-multiple");
        }
    }

    #[test]
    fn dithered_output_only_contains_16_levels() {
        // A gradient exercises the error diffusion paths.
        let mut img = RgbImage::new(64, 64);
        for (x, _y, px) in img.enumerate_pixels_mut() {
            let g = (x * 4) as u8;
            *px = image::Rgb([g, g, g]);
        }
        let opts = RenderOptions {
            screen_width: 64,
            screen_height: 64,
            fit: FitMode::Contain,
            dither: true,
        };
        let out = render_gray(&DynamicImage::ImageRgb8(img), &opts);
        assert!(out.pixels.iter().all(|p| p % 17 == 0));
    }

    #[test]
    fn dithering_preserves_average_brightness() {
        let page = solid_page(64, 64, 100);
        let opts = RenderOptions {
            screen_width: 64,
            screen_height: 64,
            fit: FitMode::Contain,
            dither: true,
        };
        let out = render_gray(&page, &opts);
        let avg: f64 = out.pixels.iter().map(|&p| p as f64).sum::<f64>() / out.pixels.len() as f64;
        assert!(
            (avg - 100.0).abs() < 4.0,
            "average {avg} drifted too far from 100"
        );
    }
}
