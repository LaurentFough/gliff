//! Fit the remote view into the window and map window points back to the
//! remote's logical pointer space. Pure arithmetic, shared by the AppKit
//! view and the tests; ported from the GTK client's `paintable::layout` and
//! `to_remote` so both clients place the picture and the pointer alike.
//!
//! Units:
//! - *points*: AppKit's logical window units (what `NSView::bounds` gives).
//! - *device pixels*: points times the window's backing scale factor.
//! - *view*: the server's full-quality fit size, in device pixels.
//! - *stream*: the coded picture size, in device pixels (<= view).
//! - *remote logical*: stream pixels divided by the stream scale, which is
//!   the space `ClientMsg::PointerMotion` is expressed in.

//= Version of this module's behaviour; bump when the mapping changes.
pub const GEOMETRY_VERSION: &str = "0.1.0";

/// Where the frame is drawn inside the view, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Device pixels per view pixel.
    pub factor: f64,
}

impl Layout {
    /// True when every view pixel covers a whole number of device pixels, so
    /// the picture can be scaled with nearest-neighbour filtering and stay
    /// sharp.
    pub fn is_integer(&self) -> bool {
        self.factor >= 1.0 && (self.factor - self.factor.round()).abs() < 1e-9
    }
}

/// The alignment step Hyprland rounds a mode to at `scale`: the smallest
/// even size a fractional scale maps to whole logical pixels. Copied from
/// `hypr-ipc::clean_mode` (that crate is Linux-only through its deps).
pub fn clean_mode(width: u32, height: u32, scale: f32) -> (u32, u32) {
    let s = f64::from(scale);
    let Some(q) = (1..=8u32).find(|q| {
        let numerator = s * f64::from(*q);
        (numerator - numerator.round()).abs() < 1e-4
    }) else {
        return (width, height);
    };
    let p = (s * f64::from(q)).round() as u32;
    let step = if p.is_multiple_of(2) { p } else { 2 * p };
    let fit = |v: u32| (v - v % step).max(step);
    (fit(width), fit(height))
}

/// Fit a `view` (device pixels) into an area of `width` x `height` points on
/// a display of `device` scale. Integer factors are preferred when the slack
/// they leave is within the compositor's alignment step, so a window the
/// server sized for us shows its pixels 1:1. Offsets snap to device pixels.
pub fn layout(view: (u32, u32), device: f64, width: f64, height: f64) -> Option<Layout> {
    let (vw, vh) = (f64::from(view.0), f64::from(view.1));
    if vw <= 0.0
        || vh <= 0.0
        || width <= 0.0
        || height <= 0.0
        || !width.is_finite()
        || !height.is_finite()
        || !device.is_finite()
        || device <= 0.0
    {
        return None;
    }
    let fit = (width * device / vw).min(height * device / vh);
    let alignment = clean_mode(2, 2, device.clamp(0.5, 4.0) as f32).0;
    let integer = fit.floor();
    let slack = (width * device - vw * integer).min(height * device - vh * integer);
    let factor = if integer >= 1.0 && slack < f64::from(alignment) + 1.0 {
        integer
    } else {
        fit
    };
    let (fw, fh) = (vw * factor / device, vh * factor / device);
    let snap = |v: f64| (v * device).floor() / device;
    Some(Layout {
        x: snap((width - fw) / 2.0),
        y: snap((height - fh) / 2.0),
        width: fw,
        height: fh,
        factor,
    })
}

/// The geometry a picture was configured under (travels with each frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamGeometry {
    pub stream: (u32, u32),
    pub view: (u32, u32),
    /// Effective stream scale x1000.
    pub scale_milli: u32,
}

/// Map a point in the view (points, top-left origin) to the remote's logical
/// pointer space. Points outside the picture clamp to its edge, as the GTK
/// client does, so dragging past the border still reaches the screen edge.
pub fn to_remote(
    point: (f64, f64),
    geom: StreamGeometry,
    device: f64,
    area: (f64, f64),
) -> Option<(f64, f64)> {
    let (rw, rh) = geom.stream;
    let (vw, vh) = geom.view;
    if rw == 0 || rh == 0 || vw == 0 || vh == 0 {
        return None;
    }
    let l = layout(geom.view, device, area.0, area.1)?;
    let vx = (point.0 - l.x) / l.factor * device;
    let vy = (point.1 - l.y) / l.factor * device;
    let px = (vx * f64::from(rw) / f64::from(vw)).clamp(0.0, f64::from(rw));
    let py = (vy * f64::from(rh) / f64::from(vh)).clamp(0.0, f64::from(rh));
    let scale = (f64::from(geom.scale_milli.max(1)) / 1000.0).max(0.01);
    Some((px / scale, py / scale))
}

/// The `Resize` a window of `width` x `height` points at `device` scale asks
/// for: even device-pixel sizes, or `None` when the area is too small to be
/// worth a remote mode (the GTK client uses the same 64 px floor).
pub fn resize_request(width: f64, height: f64, device: f64) -> Option<(u32, u32)> {
    if !(width.is_finite() && height.is_finite() && device.is_finite()) || device <= 0.0 {
        return None;
    }
    let w = (width * device).max(0.0).round() as u32 & !1;
    let h = (height * device).max(0.0).round() as u32 & !1;
    (w >= 64 && h >= 64).then_some((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_fit_is_one_to_one() {
        let l = layout((2880, 1800), 2.0, 1440.0, 900.0).unwrap();
        assert_eq!(l.factor, 1.0);
        assert!(l.is_integer());
        assert_eq!((l.x, l.y, l.width, l.height), (0.0, 0.0, 1440.0, 900.0));
    }

    #[test]
    fn a_large_view_shrinks_and_centres() {
        let l = layout((3840, 2160), 1.0, 1920.0, 1200.0).unwrap();
        assert!((l.factor - 0.5).abs() < 1e-9);
        assert!(!l.is_integer());
        assert_eq!(l.y, 60.0);
    }

    #[test]
    fn degenerate_inputs_have_no_layout() {
        assert!(layout((0, 10), 1.0, 10.0, 10.0).is_none());
        assert!(layout((10, 10), 0.0, 10.0, 10.0).is_none());
        assert!(layout((10, 10), 1.0, f64::NAN, 10.0).is_none());
    }

    #[test]
    fn pointer_maps_to_remote_logical_space() {
        //= 2560x1600 stream at scale 2.0 shown 1:1 in a 1280x800 pt window.
        let geom = StreamGeometry {
            stream: (2560, 1600),
            view: (2560, 1600),
            scale_milli: 2000,
        };
        let p = to_remote((640.0, 400.0), geom, 2.0, (1280.0, 800.0)).unwrap();
        assert_eq!(p, (640.0, 400.0));
        //= Outside the picture clamps to the edge.
        let p = to_remote((-50.0, 9000.0), geom, 2.0, (1280.0, 800.0)).unwrap();
        assert_eq!(p, (0.0, 800.0));
    }

    #[test]
    fn reduced_streams_map_through_the_view() {
        //= Server sends half resolution but a full-size view.
        let geom = StreamGeometry {
            stream: (960, 540),
            view: (1920, 1080),
            scale_milli: 500,
        };
        let p = to_remote((960.0, 540.0), geom, 1.0, (1920.0, 1080.0)).unwrap();
        assert_eq!(p, (960.0, 540.0));
    }

    #[test]
    fn resize_requests_are_even_and_bounded() {
        assert_eq!(resize_request(1001.0, 701.0, 2.0), Some((2002, 1402)));
        assert_eq!(resize_request(100.5, 100.5, 1.0), Some((100, 100)));
        assert_eq!(resize_request(20.0, 900.0, 2.0), None);
        assert_eq!(resize_request(500.0, 500.0, f64::NAN), None);
    }

    #[test]
    fn clean_mode_matches_hyprland_steps() {
        assert_eq!(clean_mode(2, 2, 1.0).0, 2);
        assert_eq!(clean_mode(2, 2, 1.5).0, 6);
        assert_eq!(clean_mode(2, 2, 1.25).0, 10);
    }
}
