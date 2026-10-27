//! The few Core Graphics / Core Animation calls the window needs, declared
//! directly against the C API. Hand-written instead of pulled from
//! `objc2-core-graphics` so they do not move with that crate's API, and
//! because the picture path wants one specific trick: a `CGImage` that wraps
//! the decoder's pooled buffer without copying it, released back to the
//! pool when Core Animation lets go of the image.

use std::ffi::c_void;
use std::sync::OnceLock;

use gliff_sw::PooledFrame;
use objc2::encode::{Encoding, RefEncode};

//= Version of the image bridge.
pub const CG_VERSION: &str = "0.1.0";

/// Opaque `CGImage`.
#[repr(C)]
pub struct CGImage {
    _private: [u8; 0],
}

// SAFETY: CGImageRef is a pointer to `struct CGImage`, which is how the
// Objective-C runtime encodes it (`^{CGImage=}`), e.g. in
// `-[NSImage initWithCGImage:size:]`.
unsafe impl RefEncode for CGImage {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGImage", &[]));
}

#[repr(C)]
struct CGColorSpace {
    _private: [u8; 0],
}

#[repr(C)]
struct CGDataProvider {
    _private: [u8; 0],
}

type ReleaseData = unsafe extern "C" fn(info: *mut c_void, data: *const c_void, size: usize);

//= CGBitmapInfo bits.
const BYTE_ORDER_32_LITTLE: u32 = 2 << 12;
const ALPHA_NONE_SKIP_FIRST: u32 = 6;
const ALPHA_FIRST: u32 = 4;
const RENDERING_INTENT_DEFAULT: i32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorSpaceSRGB: *const c_void;
    fn CGColorSpaceCreateWithName(name: *const c_void) -> *mut CGColorSpace;
    fn CGDataProviderCreateWithData(
        info: *mut c_void,
        data: *const c_void,
        size: usize,
        release: Option<ReleaseData>,
    ) -> *mut CGDataProvider;
    fn CGDataProviderRelease(provider: *mut CGDataProvider);
    fn CGImageCreate(
        width: usize,
        height: usize,
        bits_per_component: usize,
        bits_per_pixel: usize,
        bytes_per_row: usize,
        space: *mut CGColorSpace,
        bitmap_info: u32,
        provider: *mut CGDataProvider,
        decode: *const f64,
        should_interpolate: bool,
        intent: i32,
    ) -> *mut CGImage;
    fn CGImageRelease(image: *mut CGImage);
}

//= Core Animation classes (CALayer, CATransaction) come from here.
#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

/// The sRGB colour space, created once and kept for the process. Stored
/// as an address because raw pointers are not `Sync`; CGColorSpace is an
/// immutable, thread-safe CF object.
fn srgb() -> *mut CGColorSpace {
    static SPACE: OnceLock<usize> = OnceLock::new();
    // SAFETY: kCGColorSpaceSRGB is a valid CFString constant.
    *SPACE.get_or_init(|| unsafe { CGColorSpaceCreateWithName(kCGColorSpaceSRGB) } as usize)
        as *mut CGColorSpace
}

/// An owned `CGImage` reference, released on drop.
pub struct Image(*mut CGImage);

impl Image {
    pub fn as_ptr(&self) -> *mut CGImage {
        self.0
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        // SAFETY: we own one reference from CGImageCreate.
        unsafe { CGImageRelease(self.0) }
    }
}

/// What a data provider owns: dropping it frees (or re-pools) the pixels.
enum Pixels {
    Frame(PooledFrame),
    Owned(Vec<u8>),
}

impl Pixels {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Frame(f) => f.pixels.as_ref(),
            Self::Owned(v) => v,
        }
    }
}

unsafe extern "C" fn release_pixels(info: *mut c_void, _data: *const c_void, _size: usize) {
    // SAFETY: `info` is the Box<Pixels> leaked in `image_from`, released
    // exactly once by Core Graphics.
    drop(unsafe { Box::from_raw(info.cast::<Pixels>()) });
}

/// Wrap packed 32-bit BGRA pixels in a CGImage without copying them.
fn image_from(pixels: Pixels, width: u32, height: u32, alpha: u32) -> Option<Image> {
    let (w, h) = (width as usize, height as usize);
    let row = w.checked_mul(4)?;
    let len = row.checked_mul(h)?;
    if w == 0 || h == 0 || pixels.bytes().len() < len {
        tracing::warn!(
            width,
            height,
            have = pixels.bytes().len(),
            "short pixel buffer"
        );
        return None;
    }
    let data = pixels.bytes().as_ptr().cast::<c_void>();
    let info = Box::into_raw(Box::new(pixels)).cast::<c_void>();
    // SAFETY: `data` points at `len` bytes owned by `info`, which lives
    // until `release_pixels` runs; a null provider means the callback will
    // not run, so the box is reclaimed here.
    unsafe {
        let provider = CGDataProviderCreateWithData(info, data, len, Some(release_pixels));
        if provider.is_null() {
            drop(Box::from_raw(info.cast::<Pixels>()));
            return None;
        }
        let image = CGImageCreate(
            w,
            h,
            8,
            32,
            row,
            srgb(),
            BYTE_ORDER_32_LITTLE | alpha,
            provider,
            std::ptr::null(),
            false,
            RENDERING_INTENT_DEFAULT,
        );
        //= The image keeps its own reference to the provider.
        CGDataProviderRelease(provider);
        (!image.is_null()).then_some(Image(image))
    }
}

/// An opaque picture from the decoder (zero-copy).
pub fn frame_image(frame: PooledFrame) -> Option<Image> {
    let (w, h) = (frame.width, frame.height);
    image_from(Pixels::Frame(frame), w, h, ALPHA_NONE_SKIP_FIRST)
}

/// A cursor image with straight (non-premultiplied) alpha.
pub fn cursor_image(bgra: Vec<u8>, width: u32, height: u32) -> Option<Image> {
    image_from(Pixels::Owned(bgra), width, height, ALPHA_FIRST)
}
