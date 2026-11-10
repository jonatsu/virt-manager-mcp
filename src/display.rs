//! Graphical console: screenshots, keyboard input and text recognition.

use std::borrow::Cow;
use std::fmt;
use std::io::Cursor;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use image::{ImageFormat, RgbImage, imageops::FilterType};
use ocrs::{ImageSource, OcrEngine, OcrEngineParams};
use schemars::JsonSchema;
use serde::Deserialize;
use virt::domain::Domain;
use virt::stream::Stream;
use virt::sys;

use crate::config::{OCR_DETECTION_MODEL, OCR_RECOGNITION_MODEL};

/// Threads that OCR runs on, so that a wait on a busy screen leaves the host's other cores to its VMs.
pub const OCR_THREADS: usize = 4;

/// Sizes the thread pool that OCR runs in to [`OCR_THREADS`].
///
/// `ocrs` runs its models and its line recognition in rten's process-wide rayon pool, and takes no other pool: a
/// `rayon::ThreadPool::install` around a call does not bound that work. rten sizes its pool once, on first use, from
/// `RTEN_NUM_THREADS`. Nothing else in the server uses rten, so that pool is OCR's own.
///
/// # Safety
///
/// Call it before the process starts any other thread, as [`std::env::set_var`] requires.
pub unsafe fn limit_ocr_threads() {
    // SAFETY: the caller guarantees that no other thread runs yet.
    unsafe { std::env::set_var("RTEN_NUM_THREADS", OCR_THREADS.to_string()) };
}

/// Reads text lines from screen frames with the `ocrs` models in `display.ocr_models`. It is built once, and probe
/// threads share it.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by screen_text in run 2 unit 5")
)]
pub struct Recognizer {
    engine: OcrEngine,
}

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by screen_text in run 2 unit 5")
)]
impl Recognizer {
    /// Loads both models from `dir`, which `Config::validate` has checked.
    pub fn load(dir: &Path) -> Result<Self> {
        let load = |name: &str| {
            let path = dir.join(name);
            rten::Model::load_file(&path)
                .with_context(|| format!("loading OCR model {}", path.display()))
        };
        let engine = OcrEngine::new(OcrEngineParams {
            detection_model: Some(load(OCR_DETECTION_MODEL)?),
            recognition_model: Some(load(OCR_RECOGNITION_MODEL)?),
            ..OcrEngineParams::default()
        })
        .context("building the OCR engine")?;
        let threads = rten::thread_pool().run(rayon::current_num_threads);
        tracing::info!(threads, "loaded the OCR models from {}", dir.display());
        Ok(Self { engine })
    }

    /// Returns the text lines of `frame`, inverting a dark frame first.
    pub fn read_lines(&self, frame: &RgbImage) -> Result<Vec<String>> {
        self.recognise(&prepared(frame))
    }

    /// Returns the text lines of `image` as it is.
    fn recognise(&self, image: &RgbImage) -> Result<Vec<String>> {
        let source = ImageSource::from_bytes(image.as_raw(), image.dimensions())
            .context("preparing a frame for OCR")?;
        let input = self.engine.prepare_input(source)?;
        let words = self.engine.detect_words(&input)?;
        let lines = self.engine.find_text_lines(&input, &words);
        let texts = self.engine.recognize_text(&input, &lines)?;
        Ok(texts
            .into_iter()
            .flatten()
            .map(|line| line.to_string())
            .filter(|text| !text.trim().is_empty())
            .collect())
    }
}

// Probe threads share one recogniser behind an `Arc`.
const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<Recognizer>();
};

/// Returns `frame` ready for OCR: inverted when its mean luminance is below one half, because `ocrs` misreads light
/// text on a dark background, as on a text console.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by screen_text in run 2 unit 5")
)]
fn prepared(frame: &RgbImage) -> Cow<'_, RgbImage> {
    if is_dark(frame) {
        let mut inverted = frame.clone();
        image::imageops::invert(&mut inverted);
        Cow::Owned(inverted)
    } else {
        Cow::Borrowed(frame)
    }
}

/// Whether the mean luminance of `image`, with the Rec. 709 weights on the encoded values, is below one half. The
/// sum is exact integer arithmetic, so a frame at exactly one half is not dark. An empty image is not dark.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "used by screen_text in run 2 unit 5")
)]
fn is_dark(image: &RgbImage) -> bool {
    // Rec. 709 luma weights in units of 1/10000; they sum to 10000.
    const WEIGHTS: [u64; 3] = [2126, 7152, 722];
    let pixels = u64::from(image.width()) * u64::from(image.height());
    let sum: u64 = image
        .pixels()
        .map(|p| {
            WEIGHTS
                .iter()
                .zip(p.0)
                .map(|(w, c)| w * u64::from(c))
                .sum::<u64>()
        })
        .sum();
    // mean = sum / (pixels * 255 * 10000) < 1/2
    2 * sum < pixels * 255 * 10_000
}

/// A rectangle of the screen in pixels of a full-screen image the agent received, from `screenshot` without a region
/// or from a wait's attachment, with that image's size. [`Region::check`] validates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// Left edge, in pixels of the full-screen image.
    pub x: u32,
    /// Top edge, in pixels of the full-screen image.
    pub y: u32,
    /// Width in pixels of the full-screen image; at least 1.
    pub width: u32,
    /// Height in pixels of the full-screen image; at least 1.
    pub height: u32,
    /// Width of the full-screen image the coordinates come from, as its reply gave it.
    #[schemars(required)]
    pub image_width: Option<u32>,
    /// Height of the full-screen image the coordinates come from, as its reply gave it.
    #[schemars(required)]
    pub image_height: Option<u32>,
}

impl Region {
    /// Checks that the region names its image's size, has a positive size, and lies inside that image.
    pub fn check(&self) -> Result<CheckedRegion> {
        let image = ImageSize::new(self.image_width, self.image_height)?;
        if self.width == 0 || self.height == 0 {
            bail!("a region's width and height must be at least 1");
        }
        let rect = Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        };
        let fits = |start: u32, size: u32, limit: u32| {
            start.checked_add(size).is_some_and(|end| end <= limit)
        };
        if !fits(rect.x, rect.width, image.width) || !fits(rect.y, rect.height, image.height) {
            bail!("the region {rect} does not fit inside the {image} image it refers to");
        }
        Ok(CheckedRegion { rect, image })
    }
}

/// The size of a full-screen image that an agent's coordinates refer to; both sides are positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSize {
    width: u32,
    height: u32,
}

impl ImageSize {
    /// Checks the `image_width` and `image_height` an agent gave: both present and positive.
    pub fn new(width: Option<u32>, height: Option<u32>) -> Result<Self> {
        let (Some(width), Some(height)) = (width, height) else {
            bail!(
                "coordinates need image_width and image_height, the size of the full-screen image they come from"
            );
        };
        if width == 0 || height == 0 {
            bail!("image_width and image_height must be at least 1");
        }
        Ok(Self { width, height })
    }

    /// Width of the image in pixels; at least 1.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height of the image in pixels; at least 1.
    pub fn height(&self) -> u32 {
        self.height
    }
}

impl fmt::Display for ImageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// A rectangle in pixels, with a positive size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} at ({}, {})",
            self.width, self.height, self.x, self.y
        )
    }
}

/// A region that lies inside the full-screen image it refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckedRegion {
    rect: Rect,
    image: ImageSize,
}

impl CheckedRegion {
    /// The region mapped proportionally onto a frame of `width` by `height` pixels, which may differ from the image
    /// the agent saw. Edges round outwards, so the result covers the whole region, has a positive size, and never
    /// leaves the frame.
    pub fn on_frame(&self, width: u32, height: u32) -> Result<Rect> {
        if width == 0 || height == 0 {
            bail!("the screen frame is empty");
        }
        let (x, w) = scale_span(self.rect.x, self.rect.width, self.image.width, width);
        let (y, h) = scale_span(self.rect.y, self.rect.height, self.image.height, height);
        Ok(Rect {
            x,
            y,
            width: w,
            height: h,
        })
    }
}

impl fmt::Display for CheckedRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "region {} of the {} full-screen image",
            self.rect, self.image
        )
    }
}

/// Maps the span from `start` of `size` pixels on an axis of `from` pixels onto an axis of `to` pixels, rounding the
/// start down and the end up. The caller guarantees `0 < size`, `start + size <= from` and `0 < to`, so the result
/// lies inside `0..to` with a size of at least 1: the end, at least `start * to / from` plus a positive amount, rounds
/// up past the start's floor, and is at most `to`.
fn scale_span(start: u32, size: u32, from: u32, to: u32) -> (u32, u32) {
    let (from, to) = (u64::from(from), u64::from(to));
    let first = u64::from(start) * to / from;
    let end = (u64::from(start) + u64::from(size)) * to;
    let last = end.div_ceil(from).min(to);
    let narrow = |v: u64| u32::try_from(v).expect("a position on the frame fits its u32 size");
    (narrow(first), narrow(last - first))
}

/// Captures display head `screen` of `dom` at full resolution. It opens the stream on the connection `dom` came
/// from, so a probe thread holding only the domain can call it.
pub fn capture(dom: &Domain, screen: u32) -> Result<RgbImage> {
    let conn = dom
        .get_connect()
        .context("finding the domain's connection")?;
    let stream = Stream::new(&conn, 0)?;
    let mime = dom.screenshot(&stream, screen, 0).map_err(|e| {
        anyhow!(
            "screenshot failed: {} (does the VM have a graphical display?)",
            e.message()
        )
    })?;
    let mut raw = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let n = stream
            .recv(&mut chunk)
            .context("reading screenshot stream")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let _ = stream.finish();

    let format = match mime.as_str() {
        "image/png" => ImageFormat::Png,
        "image/x-portable-pixmap" => ImageFormat::Pnm,
        other => ImageFormat::from_mime_type(other)
            .ok_or_else(|| anyhow!("unsupported screenshot format {other}"))?,
    };
    Ok(image::load_from_memory_with_format(&raw, format)
        .context("decoding screenshot")?
        .into_rgb8())
}

/// Encodes `frame`, or the part `crop` names, as PNG, cropping at full resolution and then downscaling to at most
/// `max_width` pixels wide. Returns the PNG with its width and height.
pub fn encode_png(
    frame: &RgbImage,
    crop: Option<Rect>,
    max_width: Option<u32>,
) -> Result<(Vec<u8>, u32, u32)> {
    let mut img = match crop {
        Some(r) => {
            Cow::Owned(image::imageops::crop_imm(frame, r.x, r.y, r.width, r.height).to_image())
        }
        None => Cow::Borrowed(frame),
    };
    if let Some(w) = max_width
        && w > 0
        && img.width() > w
    {
        let h = (u64::from(img.height()) * u64::from(w) / u64::from(img.width())).max(1);
        let h = u32::try_from(h).expect("a downscaled height is below the original");
        img = Cow::Owned(image::imageops::resize(&*img, w, h, FilterType::Triangle));
    }
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    Ok((png, img.width(), img.height()))
}

/// The number of pixels in which `a` and `b` differ, compared exactly, inside `area` or over the whole frame. Returns
/// `None` when the frames differ in size, which counts as a change of every pixel. The caller maps `area` onto the
/// frame, so it lies inside both frames when their sizes match.
pub fn changed_pixels(a: &RgbImage, b: &RgbImage, area: Option<Rect>) -> Option<u64> {
    if a.dimensions() != b.dimensions() {
        return None;
    }
    let area = area.unwrap_or(Rect {
        x: 0,
        y: 0,
        width: a.width(),
        height: a.height(),
    });
    let mut changed = 0;
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            if a.get_pixel(x, y) != b.get_pixel(x, y) {
                changed += 1;
            }
        }
    }
    Some(changed)
}

/// Presses a set of keys simultaneously (e.g. ctrl+alt+delete) and releases them.
pub fn send_combo(dom: &Domain, keys: &[u32], hold_ms: u32) -> Result<()> {
    if keys.is_empty() {
        bail!("no keys given");
    }
    if keys.len() > sys::VIR_DOMAIN_SEND_KEY_MAX_KEYS as usize {
        bail!(
            "at most {} keys per combo",
            sys::VIR_DOMAIN_SEND_KEY_MAX_KEYS
        );
    }
    let mut codes = keys.to_vec();
    dom.send_key(
        sys::VIR_KEYCODE_SET_LINUX,
        hold_ms,
        codes.as_mut_ptr(),
        codes.len() as i32,
        0,
    )
    .map_err(|e| anyhow!("send_key failed: {}", e.message()))
}

/// Parses "ctrl+alt+delete", "Enter", "shift+a", ... into Linux keycodes.
pub fn parse_combo(combo: &str) -> Result<Vec<u32>> {
    combo
        .split('+')
        .map(|k| {
            let k = k.trim();
            key_by_name(k).ok_or_else(|| anyhow!("unknown key '{k}' in '{combo}'"))
        })
        .collect()
}

/// Types text using a US keyboard layout.
pub fn type_text(dom: &Domain, text: &str, delay: Duration) -> Result<()> {
    // Validate everything up front so we never type half a string.
    let text = text.replace("\r\n", "\n");
    let strokes: Vec<Vec<u32>> = text
        .chars()
        .map(|c| {
            char_to_keys(c).ok_or_else(|| {
                anyhow!("character {c:?} cannot be typed with a US layout; use exec, write_file or console_write instead")
            })
        })
        .collect::<Result<_>>()?;
    for keys in strokes {
        send_combo(dom, &keys, 10)?;
        std::thread::sleep(delay);
    }
    Ok(())
}

// Linux input event codes (linux/input-event-codes.h).
const KEY_LEFTSHIFT: u32 = 42;

fn key_by_name(name: &str) -> Option<u32> {
    let n = name.to_ascii_lowercase();
    if n.chars().count() == 1 {
        let c = n.chars().next()?;
        if let Some((code, false)) = char_key(c) {
            return Some(code);
        }
    }
    if let Some(f) = n.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()) {
        return match f {
            1..=10 => Some(58 + f),
            11 => Some(87),
            12 => Some(88),
            _ => None,
        };
    }
    Some(match n.as_str() {
        "esc" | "escape" => 1,
        "backspace" => 14,
        "tab" => 15,
        "enter" | "return" => 28,
        "ctrl" | "control" | "lctrl" => 29,
        "shift" | "lshift" => KEY_LEFTSHIFT,
        "rshift" => 54,
        "alt" | "lalt" => 56,
        "space" => 57,
        "capslock" => 58,
        "numlock" => 69,
        "scrolllock" => 70,
        "rctrl" => 97,
        "sysrq" | "printscreen" | "print" => 99,
        "ralt" | "altgr" => 100,
        "home" => 102,
        "up" => 103,
        "pageup" | "pgup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" | "pgdn" => 109,
        "insert" | "ins" => 110,
        "delete" | "del" => 111,
        "pause" => 119,
        "meta" | "super" | "win" | "windows" | "lmeta" => 125,
        "rmeta" => 126,
        "menu" | "compose" => 127,
        "minus" => 12,
        "equal" => 13,
        _ => return None,
    })
}

fn char_to_keys(c: char) -> Option<Vec<u32>> {
    let (code, shift) = char_key(c)?;
    Some(if shift {
        vec![KEY_LEFTSHIFT, code]
    } else {
        vec![code]
    })
}

/// Maps a character to (keycode, needs_shift) for a US layout.
fn char_key(c: char) -> Option<(u32, bool)> {
    const ROW_QWERTY: [(char, u32); 10] = [
        ('q', 16),
        ('w', 17),
        ('e', 18),
        ('r', 19),
        ('t', 20),
        ('y', 21),
        ('u', 22),
        ('i', 23),
        ('o', 24),
        ('p', 25),
    ];
    const ROW_ASDF: [(char, u32); 9] = [
        ('a', 30),
        ('s', 31),
        ('d', 32),
        ('f', 33),
        ('g', 34),
        ('h', 35),
        ('j', 36),
        ('k', 37),
        ('l', 38),
    ];
    const ROW_ZXCV: [(char, u32); 7] = [
        ('z', 44),
        ('x', 45),
        ('c', 46),
        ('v', 47),
        ('b', 48),
        ('n', 49),
        ('m', 50),
    ];
    // (unshifted, shifted, code)
    const SYMBOLS: [(char, char, u32); 21] = [
        ('1', '!', 2),
        ('2', '@', 3),
        ('3', '#', 4),
        ('4', '$', 5),
        ('5', '%', 6),
        ('6', '^', 7),
        ('7', '&', 8),
        ('8', '*', 9),
        ('9', '(', 10),
        ('0', ')', 11),
        ('-', '_', 12),
        ('=', '+', 13),
        ('[', '{', 26),
        (']', '}', 27),
        (';', ':', 39),
        ('\'', '"', 40),
        ('`', '~', 41),
        ('\\', '|', 43),
        (',', '<', 51),
        ('.', '>', 52),
        ('/', '?', 53),
    ];
    match c {
        ' ' => return Some((57, false)),
        '\n' | '\r' => return Some((28, false)),
        '\t' => return Some((15, false)),
        _ => {}
    }
    let lower = c.to_ascii_lowercase();
    for (ch, code) in ROW_QWERTY.iter().chain(&ROW_ASDF).chain(&ROW_ZXCV) {
        if *ch == lower {
            return Some((*code, c.is_ascii_uppercase()));
        }
    }
    SYMBOLS.iter().find_map(|&(plain, shifted, code)| {
        if c == plain {
            Some((code, false))
        } else if c == shifted {
            Some((code, true))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combos() {
        assert_eq!(parse_combo("ctrl+alt+delete").unwrap(), vec![29, 56, 111]);
        assert_eq!(parse_combo("Win+R").unwrap(), vec![125, 19]);
        assert_eq!(parse_combo("F12").unwrap(), vec![88]);
        assert_eq!(parse_combo("f1").unwrap(), vec![59]);
        assert!(parse_combo("ctrl+nope").is_err());
    }

    #[test]
    fn chars() {
        assert_eq!(char_to_keys('a'), Some(vec![30]));
        assert_eq!(char_to_keys('A'), Some(vec![KEY_LEFTSHIFT, 30]));
        assert_eq!(char_to_keys('|'), Some(vec![KEY_LEFTSHIFT, 43]));
        assert_eq!(char_to_keys('\n'), Some(vec![28]));
        assert_eq!(char_to_keys('é'), None);
    }

    /// A `width`x1 image whose first `white` pixels are white and the rest black.
    fn stripe(width: u32, white: u32) -> RgbImage {
        RgbImage::from_fn(width, 1, |x, _| {
            if x < white {
                image::Rgb([255, 255, 255])
            } else {
                image::Rgb([0, 0, 0])
            }
        })
    }

    #[test]
    fn only_frames_darker_than_one_half_are_inverted() {
        let dark = stripe(10, 4);
        let inverted = prepared(&dark);
        assert!(matches!(inverted, Cow::Owned(_)));
        let expected = RgbImage::from_fn(10, 1, |x, _| {
            if x < 4 {
                image::Rgb([0, 0, 0])
            } else {
                image::Rgb([255, 255, 255])
            }
        });
        assert_eq!(*inverted, expected);

        // Exactly one half is not below it.
        assert!(matches!(prepared(&stripe(10, 5)), Cow::Borrowed(_)));
        assert!(matches!(prepared(&stripe(10, 9)), Cow::Borrowed(_)));
        assert!(matches!(prepared(&RgbImage::new(0, 0)), Cow::Borrowed(_)));
    }

    #[test]
    fn luminance_weights_green_most() {
        let solid = |rgb| RgbImage::from_pixel(2, 2, image::Rgb(rgb));
        assert!(!is_dark(&solid([255, 255, 255])));
        assert!(is_dark(&solid([0, 0, 0])));
        // Pure green is light and pure red and blue are dark, so a blue screen is inverted and a green one is not.
        assert!(!is_dark(&solid([0, 255, 0])));
        assert!(is_dark(&solid([255, 0, 0])));
        assert!(is_dark(&solid([0, 0, 255])));
        // Mid-grey: 128 is just above one half of 255, 127 just below.
        assert!(!is_dark(&solid([128, 128, 128])));
        assert!(is_dark(&solid([127, 127, 127])));
    }

    fn region(x: u32, y: u32, width: u32, height: u32, image: (u32, u32)) -> Region {
        Region {
            x,
            y,
            width,
            height,
            image_width: Some(image.0),
            image_height: Some(image.1),
        }
    }

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// The region mapped onto a frame of `frame` size.
    fn mapped(r: Region, frame: (u32, u32)) -> Rect {
        r.check().unwrap().on_frame(frame.0, frame.1).unwrap()
    }

    fn refusal(r: Region) -> String {
        format!("{:#}", r.check().unwrap_err())
    }

    #[test]
    fn a_frame_of_the_image_size_maps_corners_to_themselves() {
        let image = (1280, 800);
        assert_eq!(mapped(region(0, 0, 1, 1, image), image), rect(0, 0, 1, 1));
        assert_eq!(
            mapped(region(1279, 799, 1, 1, image), image),
            rect(1279, 799, 1, 1)
        );
        assert_eq!(
            mapped(region(0, 0, 1280, 800, image), image),
            rect(0, 0, 1280, 800)
        );
    }

    #[test]
    fn a_larger_frame_scales_the_region_up() {
        let image = (640, 400);
        let frame = (1280, 800);
        assert_eq!(
            mapped(region(10, 20, 30, 40, image), frame),
            rect(20, 40, 60, 80)
        );
        // The bottom-right pixel of the image covers two by two pixels of the frame.
        assert_eq!(
            mapped(region(639, 399, 1, 1, image), frame),
            rect(1278, 798, 2, 2)
        );
    }

    #[test]
    fn a_smaller_frame_scales_the_region_down() {
        let image = (1280, 800);
        let frame = (720, 400);
        assert_eq!(
            mapped(region(0, 0, 640, 400, image), frame),
            rect(0, 0, 360, 200)
        );
        assert_eq!(
            mapped(region(640, 400, 640, 400, image), frame),
            rect(360, 200, 360, 200)
        );
    }

    #[test]
    fn regions_at_the_right_and_bottom_edges_stay_inside_the_frame() {
        let image = (1280, 800);
        let frame = (720, 400);
        // 1279 * 720 / 1280 = 719.4: the start rounds down, the end up to the frame's edge.
        assert_eq!(
            mapped(region(1279, 0, 1, 800, image), frame),
            rect(719, 0, 1, 400)
        );
        assert_eq!(
            mapped(region(0, 799, 1280, 1, image), frame),
            rect(0, 399, 720, 1)
        );
        // A frame smaller than one image pixel per frame pixel still gives a pixel.
        assert_eq!(mapped(region(1, 1, 1, 1, (3, 3)), (2, 2)), rect(0, 0, 2, 2));
    }

    /// Every region of every small image, on every small frame, maps to a non-empty rectangle inside the frame that
    /// covers the region's proportional span.
    #[test]
    fn rounding_never_leaves_the_frame() {
        for from in 1..=7u32 {
            for to in 1..=7u32 {
                for start in 0..from {
                    for size in 1..=from - start {
                        let (first, len) = scale_span(start, size, from, to);
                        assert!(len >= 1, "{start}+{size} of {from} on {to}");
                        assert!(first + len <= to, "{start}+{size} of {from} on {to}");
                        assert!(first * from <= start * to);
                        assert!((first + len) * from >= (start + size) * to);
                    }
                }
            }
        }
    }

    #[test]
    fn a_region_needs_the_size_of_its_image() {
        let mut r = region(0, 0, 10, 10, (100, 100));
        r.image_width = None;
        assert!(refusal(r).contains("need image_width and image_height"));
        let mut r = region(0, 0, 10, 10, (100, 100));
        r.image_height = None;
        assert!(refusal(r).contains("need image_width and image_height"));
        assert!(refusal(region(0, 0, 10, 10, (0, 100))).contains("must be at least 1"));
        assert!(refusal(region(0, 0, 10, 10, (100, 0))).contains("must be at least 1"));

        let parsed: Region =
            serde_json::from_str(r#"{"x": 0, "y": 0, "width": 10, "height": 10}"#).unwrap();
        assert!(refusal(parsed).contains("need image_width and image_height"));
        let schema = serde_json::to_value(schemars::schema_for!(Region)).unwrap();
        let required = schema["required"].as_array().unwrap();
        for field in ["image_width", "image_height"] {
            assert!(required.contains(&field.into()), "{field} in {required:?}");
        }
    }

    #[test]
    fn a_region_needs_a_positive_size() {
        assert!(
            refusal(region(0, 0, 0, 10, (100, 100)))
                .contains("width and height must be at least 1")
        );
        assert!(
            refusal(region(0, 0, 10, 0, (100, 100)))
                .contains("width and height must be at least 1")
        );
    }

    #[test]
    fn a_region_must_lie_inside_its_image() {
        let image = (1280, 800);
        assert!(region(1270, 790, 10, 10, image).check().is_ok());
        assert_eq!(
            refusal(region(1271, 0, 10, 10, image)),
            "the region 10x10 at (1271, 0) does not fit inside the 1280x800 image it refers to"
        );
        assert!(refusal(region(0, 791, 10, 10, image)).contains("does not fit inside"));
        assert!(refusal(region(1280, 0, 1, 1, image)).contains("does not fit inside"));
        // An end past u32::MAX is outside, not a wrapped small number.
        assert!(refusal(region(u32::MAX, 0, 2, 1, image)).contains("does not fit inside"));
    }

    #[test]
    fn an_empty_frame_is_refused() {
        let checked = region(0, 0, 1, 1, (10, 10)).check().unwrap();
        assert!(checked.on_frame(0, 10).is_err());
        assert!(checked.on_frame(10, 0).is_err());
    }

    #[test]
    fn the_region_reply_names_its_origin_size_and_image() {
        let checked = region(5, 6, 7, 8, (1280, 800)).check().unwrap();
        assert_eq!(
            checked.to_string(),
            "region 7x8 at (5, 6) of the 1280x800 full-screen image"
        );
    }

    fn decoded(png: &[u8]) -> RgbImage {
        image::load_from_memory_with_format(png, ImageFormat::Png)
            .unwrap()
            .into_rgb8()
    }

    #[test]
    fn encoding_crops_at_full_resolution_before_downscaling() {
        // Red on the left half, blue on the right, with a one-pixel green column at x = 60.
        let frame = RgbImage::from_fn(100, 50, |x, _| match x {
            60 => image::Rgb([0, 255, 0]),
            0..50 => image::Rgb([255, 0, 0]),
            _ => image::Rgb([0, 0, 255]),
        });

        let (png, w, h) = encode_png(&frame, Some(rect(60, 10, 1, 20)), None).unwrap();
        assert_eq!((w, h), (1, 20));
        assert!(decoded(&png).pixels().all(|p| p.0 == [0, 255, 0]));

        let (png, w, h) = encode_png(&frame, Some(rect(70, 0, 30, 50)), Some(10)).unwrap();
        assert_eq!((w, h), (10, 16));
        assert!(decoded(&png).pixels().all(|p| p.0 == [0, 0, 255]));

        // A crop already narrower than max_width keeps its size.
        let (_, w, h) = encode_png(&frame, Some(rect(0, 0, 40, 20)), Some(1280)).unwrap();
        assert_eq!((w, h), (40, 20));
    }

    #[test]
    fn encoding_without_a_region_keeps_the_full_screenshot_behaviour() {
        let frame = RgbImage::from_pixel(1600, 900, image::Rgb([1, 2, 3]));
        let (png, w, h) = encode_png(&frame, None, Some(1280)).unwrap();
        assert_eq!((w, h), (1280, 720));
        assert_eq!(decoded(&png).dimensions(), (1280, 720));
        assert_eq!(encode_png(&frame, None, None).unwrap().1, 1600);
        assert_eq!(encode_png(&frame, None, Some(0)).unwrap().1, 1600);
    }

    #[test]
    fn changed_pixels_counts_exact_differences_in_the_area() {
        let a = RgbImage::from_pixel(10, 10, image::Rgb([9, 9, 9]));
        let mut b = a.clone();
        // One channel off by one is a change.
        b.put_pixel(0, 0, image::Rgb([9, 9, 10]));
        b.put_pixel(5, 5, image::Rgb([0, 0, 0]));
        b.put_pixel(9, 9, image::Rgb([0, 0, 0]));
        assert_eq!(changed_pixels(&a, &a, None), Some(0));
        assert_eq!(changed_pixels(&a, &b, None), Some(3));
        assert_eq!(changed_pixels(&a, &b, Some(rect(5, 5, 5, 5))), Some(2));
        assert_eq!(changed_pixels(&a, &b, Some(rect(1, 1, 4, 4))), Some(0));
        assert_eq!(changed_pixels(&a, &b, Some(rect(9, 9, 1, 1))), Some(1));
        // A size change is not a pixel count.
        assert_eq!(changed_pixels(&a, &RgbImage::new(10, 11), None), None);
        assert_eq!(
            changed_pixels(&a, &RgbImage::new(11, 10), Some(rect(0, 0, 1, 1))),
            None
        );
    }

    // Tests with the OCR models. They read the probe's screenshots in docs/research/2026-11-10-ocr-probe/. Run them
    // with `VIRTMANAGER_OCR_MODELS=<dir> RTEN_NUM_THREADS=4 cargo test -- --ignored --nocapture`; the second variable
    // gives rten the pool size the server sets, so the printed times match the server's.

    fn models_recognizer() -> Recognizer {
        let dir = std::env::var_os("VIRTMANAGER_OCR_MODELS").expect(
            "set VIRTMANAGER_OCR_MODELS to the directory scripts/fetch-ocr-models.sh filled",
        );
        Recognizer::load(Path::new(&dir)).unwrap()
    }

    fn probe_screen(name: &str) -> RgbImage {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("docs/research/2026-11-10-ocr-probe")
            .join(name);
        image::open(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
            .into_rgb8()
    }

    /// The probe's key strings for each saved screenshot, with the number that `ocrs` 0.12.2 read exactly on the
    /// full frame, from the recognition table of docs/research/2026-11-10-ocr-probe.md. The location and keyboard
    /// screens of the installer have no saved screenshot, so their six strings, all read exactly, are left out.
    const PROBE_KEYS: [(&str, usize, &[&str]); 10] = [
        (
            "deb13-console-login.png",
            1,
            &["Debian GNU/Linux 13", "trial-deb13 login:", "tty1"],
        ),
        (
            "vga-text-boot.png",
            0,
            &[
                "SeaBIOS",
                "Booting from Hard Disk...",
                "GRUB loading.",
                "Welcome to GRUB!",
            ],
        ),
        (
            "deb-installer-grub.png",
            1,
            &[
                "Debian GNU/Linux 13.7.0",
                "UEFI Installer menu",
                "Graphical install",
                "Advanced options ...",
                "Install with speech synthesis",
                "GRUB Command line",
            ],
        ),
        (
            "deb-installer-language.png",
            4,
            &["Select a language", "English", "Continue", "Go Back"],
        ),
        (
            "deb-installer-progress.png",
            3,
            &[
                "Load installer components from installation media",
                "Loading additional components",
                "Retrieving grub-installer",
            ],
        ),
        (
            "deb-installer-hostname.png",
            2,
            &["Hostname:", "debianxxxxxxxxxx", "Continue"],
        ),
        (
            "win11-press-any-key.png",
            1,
            &["Press any key to boot from CD or DVD"],
        ),
        (
            "win11-setup-language.png",
            5,
            &[
                "Windows 11 Setup",
                "Select language settings",
                "Language to install",
                "English (United States)",
                "Next",
            ],
        ),
        (
            "win11-setup-keyboard.png",
            4,
            &[
                "Select keyboard settings",
                "Keyboard or input method",
                "US",
                "Next",
            ],
        ),
        (
            "win11-setup-option.png",
            4,
            &[
                "Select setup option",
                "Install Windows 11",
                "Repair my PC",
                "I agree everything will be deleted",
                "Next",
            ],
        ),
    ];

    /// `ocrs` 0.12.2 read 25 of the 38 key strings on these 10 screens exactly (31 of 44 with the two screens left
    /// out). The engine must read at least as many without inversion. A read is exact when the string occurs,
    /// case-sensitively, in the recognised lines joined by newlines, as `screen_text` matches them.
    #[test]
    #[ignore = "needs the OCR models; set VIRTMANAGER_OCR_MODELS"]
    fn raw_reads_match_ocrs_0_12_on_the_probe_screens() {
        const BASELINE_0_12: usize = 25;
        assert_eq!(
            PROBE_KEYS.iter().map(|(_, n, _)| n).sum::<usize>(),
            BASELINE_0_12
        );
        let recognizer = models_recognizer();
        println!(
            "rten pool: {} threads",
            rten::thread_pool().run(rayon::current_num_threads)
        );
        let mut total = 0;
        for (name, baseline, keys) in PROBE_KEYS {
            let frame = probe_screen(name);
            let started = std::time::Instant::now();
            let text = recognizer.recognise(&frame).unwrap().join("\n");
            let elapsed = started.elapsed();
            let missed: Vec<_> = keys.iter().filter(|k| !text.contains(*k)).collect();
            let exact = keys.len() - missed.len();
            total += exact;
            println!(
                "{name}: {exact} of {} exact (0.12: {baseline}), {} ms; missed {missed:?}\n  {text:?}",
                keys.len(),
                elapsed.as_millis()
            );
        }
        println!("total: {total} exact (0.12: {BASELINE_0_12})");
        assert!(
            total >= BASELINE_0_12,
            "read {total} key strings exactly, below ocrs 0.12's {BASELINE_0_12}"
        );
    }

    /// Every string a run-2 test-bed case waits for on a saved screenshot, read through the recogniser with
    /// inversion. "Select your location" has no saved screenshot; the live case of run 2 unit 7 checks it.
    #[test]
    #[ignore = "needs the OCR models; set VIRTMANAGER_OCR_MODELS"]
    fn recognizer_reads_the_case_strings() {
        let recognizer = models_recognizer();
        for (name, wanted) in [
            ("deb-installer-grub.png", "Graphical install"),
            ("deb-installer-language.png", "Select a language"),
            ("win11-setup-language.png", "Windows 11 Setup"),
        ] {
            let started = std::time::Instant::now();
            let text = recognizer
                .read_lines(&probe_screen(name))
                .unwrap()
                .join("\n");
            println!("{name}: {} ms\n  {text:?}", started.elapsed().as_millis());
            assert!(text.contains(wanted), "{name}: no {wanted:?} in {text:?}");
        }
    }
}
