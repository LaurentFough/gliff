//! Cursor image buffers: a wl_shm pool over a memfd, read back with `read_at`.

use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;

use nix::sys::memfd::{memfd_create, MFdFlags};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::{Dispatch, QueueHandle};

use crate::{Error, Result};

/// Byte layout of a packed 32-bit wl_shm buffer. wl_shm takes the pool size,
/// the dimensions and the stride as `i32`, so a buffer whose size does not
/// fit is an error rather than a wrapped pool.
pub struct ShmLayout {
    pub width: i32,
    pub height: i32,
    pub stride: i32,
    pub size: i32,
    /// `stride` and `size` unsigned, for the readers and `set_len`.
    pub row_bytes: u32,
    pub len: u64,
}

impl ShmLayout {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let too_large = || Error::Capture(format!("{width}x{height} is too large for wl_shm"));
        let row_bytes = width.checked_mul(4).ok_or_else(too_large)?;
        let stride = i32::try_from(row_bytes).map_err(|_| too_large())?;
        let height_i32 = i32::try_from(height).map_err(|_| too_large())?;
        let size = stride.checked_mul(height_i32).ok_or_else(too_large)?;
        Ok(Self {
            width: stride / 4,
            height: height_i32,
            stride,
            size,
            row_bytes,
            len: u64::from(row_bytes) * u64::from(height),
        })
    }
}

pub struct ShmBuffer {
    file: File,
    pool: WlShmPool,
    pub buffer: WlBuffer,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl ShmBuffer {
    pub fn new<D>(shm: &WlShm, qh: &QueueHandle<D>, width: u32, height: u32) -> Result<Self>
    where
        D: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
    {
        let layout = ShmLayout::new(width, height)?;
        let fd = memfd_create(c"gliff-cursor", MFdFlags::MFD_CLOEXEC)
            .map_err(|e| Error::Capture(format!("memfd: {e}")))?;
        let file = File::from(fd);
        file.set_len(layout.len)?;
        let pool = shm.create_pool(file.as_fd(), layout.size, qh, ());
        let buffer = pool.create_buffer(
            0,
            layout.width,
            layout.height,
            layout.stride,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        Ok(Self {
            file,
            pool,
            buffer,
            width,
            height,
            stride: layout.row_bytes,
        })
    }

    /// Copy the pixels out as tightly packed ARGB8888 (little-endian BGRA bytes).
    pub fn read_argb(&self) -> Result<Vec<u8>> {
        let mut out = vec![0u8; (self.width * self.height * 4) as usize];
        for row in 0..self.height as usize {
            let off = row as u64 * self.stride as u64;
            let dst = &mut out[row * self.width as usize * 4..(row + 1) * self.width as usize * 4];
            self.file.read_exact_at(dst, off)?;
        }
        Ok(out)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_packs_four_bytes_per_pixel() {
        let l = ShmLayout::new(1920, 1080).unwrap();
        assert_eq!((l.width, l.height, l.stride), (1920, 1080, 7680));
        assert_eq!(
            (l.size, l.row_bytes, l.len),
            (7680 * 1080, 7680, 7680 * 1080)
        );
    }

    #[test]
    fn layout_rejects_sizes_wl_shm_cannot_take() {
        // The pool size overflows i32, the stride overflows u32, and the
        // height alone does not fit in an i32.
        assert!(ShmLayout::new(65536, 65536).is_err());
        assert!(ShmLayout::new(u32::MAX, 1).is_err());
        assert!(ShmLayout::new(1, u32::MAX).is_err());
    }
}
