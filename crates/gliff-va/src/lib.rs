//! VA-API (libva) H.264 encode and decode on surfaces the driver owns.
//!
//! The surfaces are exported as dmabufs so the Vulkan compute stages in
//! `gliff-vk` write the encoder's input and read the decoder's output in
//! place. libva calls are `unsafe` through the generated bindings; callers
//! see plain Rust types.

mod bindings;
pub mod context;
pub mod decoder;
pub mod display;
pub mod encoder;
pub mod h264;
pub mod settings;
pub mod surface;

pub use decoder::H264Decoder;
pub use display::{Caps, Display};
pub use encoder::{EncodedPacket, H264Encoder, PendingEncode};
pub use settings::EncoderSettings;
pub use surface::{PrimeDescriptor, PrimePlane, Surface, UsageHint};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{call}: {message} ({status:#x})")]
    Va {
        call: &'static str,
        status: i32,
        message: String,
    },
    #[error("no VA-API device: {0}")]
    NoDevice(String),
    #[error("bitstream: {0}")]
    Bitstream(&'static str),
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The bitstream's picture size differs from the announced stream size.
    #[error("stream size: {0}")]
    StreamSize(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Turn a libva status into a `Result`.
pub(crate) fn check(status: bindings::VAStatus, call: &'static str) -> Result<()> {
    if status == bindings::VA_STATUS_SUCCESS as bindings::VAStatus {
        return Ok(());
    }
    // SAFETY: vaErrorStr returns a pointer to a static string for any status.
    let message = unsafe {
        std::ffi::CStr::from_ptr(bindings::vaErrorStr(status))
            .to_string_lossy()
            .into_owned()
    };
    Err(Error::Va {
        call,
        status,
        message,
    })
}
