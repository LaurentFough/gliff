//! Fit the remote view to the window using the surface's actual device scale.
//! Integer decoder zoom keeps exact integer enlargement sharp; GTK handles the
//! remaining fractional fit so mirrored screens fill the available picture area.

use std::cell::{Cell, RefCell};

use gtk::gdk;
use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk4 as gtk;

/// Where the frame is drawn inside the widget, in logical pixels.
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
    /// The integer zoom to ask of the decoder: the factor when the view
    /// fits, else 1 (a shrunk frame is resampled anyway).
    pub fn zoom(&self) -> u32 {
        if self.factor >= 1.0 {
            self.factor as u32
        } else {
            1
        }
    }
}

/// Fit a `view` (device pixels) into a widget of `width` x `height` logical
/// pixels on a display of `device` scale. Offsets are snapped to device pixels.
pub fn layout(view: (u32, u32), device: f64, width: f64, height: f64) -> Option<Layout> {
    let (vw, vh) = (view.0 as f64, view.1 as f64);
    if vw <= 0.0
        || vh <= 0.0
        || width <= 0.0
        || height <= 0.0
        || !device.is_finite()
        || device <= 0.0
    {
        return None;
    }
    let fit = (width * device / vw).min(height * device / vh);
    // The smallest even mode gives the compositor's alignment step at this
    // scale. Allow that much slack plus rounding when preserving exact integer
    // sampling; fractional scales can require more than ten device pixels.
    let alignment = hypr_ipc::clean_mode(2, 2, device.clamp(0.5, 4.0) as f32).0;
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

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FramePaintable {
        pub texture: RefCell<Option<gdk::Texture>>,
        pub scale: Cell<f64>,
        /// The full-quality fit size in device pixels; the texture is
        /// stretched to it when the stream is scaled down.
        pub view: Cell<(u32, u32)>,
    }

    impl FramePaintable {
        fn view_or_texture(&self) -> (i32, i32) {
            let (vw, vh) = self.view.get();
            if vw > 0 && vh > 0 {
                return (vw as i32, vh as i32);
            }
            self.texture
                .borrow()
                .as_ref()
                .map_or((0, 0), |t| (t.width(), t.height()))
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FramePaintable {
        const NAME: &'static str = "GliffFramePaintable";
        type Type = super::FramePaintable;
        type Interfaces = (gdk::Paintable,);
    }

    impl ObjectImpl for FramePaintable {}

    impl PaintableImpl for FramePaintable {
        fn intrinsic_width(&self) -> i32 {
            (self.view_or_texture().0 as f64 / self.scale.get().max(f64::EPSILON)).round() as i32
        }

        fn intrinsic_height(&self) -> i32 {
            (self.view_or_texture().1 as f64 / self.scale.get().max(f64::EPSILON)).round() as i32
        }

        fn intrinsic_aspect_ratio(&self) -> f64 {
            let (w, h) = self.view_or_texture();
            if h == 0 {
                return 0.0;
            }
            w as f64 / h as f64
        }

        fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
            let Some(t) = self.texture.borrow().clone() else {
                return;
            };
            let (vw, vh) = self.view_or_texture();
            let Some(l) = layout((vw as u32, vh as u32), self.scale.get(), width, height) else {
                return;
            };
            let bounds =
                graphene::Rect::new(l.x as f32, l.y as f32, l.width as f32, l.height as f32);
            snapshot.append_texture(&t, &bounds);
        }
    }
}

glib::wrapper! {
    pub struct FramePaintable(ObjectSubclass<imp::FramePaintable>) @implements gdk::Paintable;
}

impl Default for FramePaintable {
    fn default() -> Self {
        let p: Self = glib::Object::new();
        p.imp().scale.set(1.0);
        p
    }
}

impl FramePaintable {
    /// Show `texture` inside a `view`-sized area (device pixels) on a
    /// display of `scale_factor`.
    pub fn set_frame(&self, texture: gdk::Texture, scale_factor: f64, view: (u32, u32)) {
        let imp = self.imp();
        let size_changed = imp.scale.get() != scale_factor || imp.view.get() != view;
        imp.scale.set(scale_factor);
        imp.view.set(view);
        *imp.texture.borrow_mut() = Some(texture);
        if size_changed {
            self.invalidate_size();
        }
        self.invalidate_contents();
    }

    /// Follow a display scale change without waiting for a new frame, so
    /// the drawn rectangle and the pointer mapping agree.
    pub fn set_scale(&self, scale_factor: f64) {
        let imp = self.imp();
        if imp.scale.get() != scale_factor {
            imp.scale.set(scale_factor);
            self.invalidate_size();
            self.invalidate_contents();
        }
    }

    /// Show nothing, as when no machine is connected.
    pub fn clear(&self) {
        let imp = self.imp();
        imp.view.set((0, 0));
        if imp.texture.borrow_mut().take().is_some() {
            self.invalidate_size();
            self.invalidate_contents();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_when_the_widget_has_room() {
        let l = layout((2560, 1440), 2.0, 2560.0, 1440.0).unwrap();
        assert_eq!(l.factor, 2.0);
        assert_eq!((l.x, l.y, l.width, l.height), (0.0, 0.0, 2560.0, 1440.0));
    }

    #[test]
    fn fits_when_two_does_not_fit() {
        let l = layout((2560, 1440), 2.0, 2560.0, 1400.0).unwrap();
        assert!((l.height - 1400.0).abs() < 0.001);
        assert!((l.width - 2488.888888).abs() < 0.001);
        assert_eq!(l.y, 0.0);
    }

    #[test]
    fn shrinks_fractionally_when_too_small() {
        let l = layout((2560, 1440), 1.0, 1280.0, 1000.0).unwrap();
        assert_eq!(l.factor, 0.5);
        assert_eq!((l.width, l.height), (1280.0, 720.0));
        assert_eq!((l.x, l.y), (0.0, 140.0));
    }

    #[test]
    fn offsets_snap_to_device_pixels() {
        let l = layout((1001, 601), 2.0, 1000.0, 600.0).unwrap();
        assert_eq!(l.x * 2.0, (l.x * 2.0).floor());
        assert_eq!(l.y * 2.0, (l.y * 2.0).floor());
    }

    #[test]
    fn zoom_is_the_integer_factor_or_one() {
        assert_eq!(layout((2560, 1440), 2.0, 2560.0, 1440.0).unwrap().zoom(), 2);
        assert_eq!(layout((2560, 1440), 2.0, 2560.0, 1400.0).unwrap().zoom(), 1);
        assert_eq!(layout((2560, 1440), 1.0, 1280.0, 1000.0).unwrap().zoom(), 1);
    }

    #[test]
    fn ultrawide_mirror_fills_matching_window() {
        let l = layout((3840, 1080), 1.0, 5120.0, 1440.0).unwrap();
        assert_eq!((l.width, l.height), (5120.0, 1440.0));
        assert_eq!((l.x, l.y), (0.0, 0.0));
    }

    #[test]
    fn fractional_surface_scale_fills_the_window() {
        let l = layout((1920, 1080), 1.2, 1600.0, 900.0).unwrap();
        assert_eq!((l.width, l.height), (1600.0, 900.0));
        assert_eq!((l.x, l.y), (0.0, 0.0));
        assert_eq!(l.zoom(), 1);
    }

    #[test]
    fn nearly_exact_modes_keep_integer_sampling() {
        for (device, width, height) in [(1.0, 1601.0, 901.0), (1.25, 1287.0, 727.0)] {
            let l = layout((1600, 900), device, width, height).unwrap();
            assert_eq!(l.factor, 1.0);
            assert_eq!(l.width * device, 1600.0);
            assert_eq!(l.height * device, 900.0);
        }
    }

    #[test]
    fn mode_alignment_slack_tracks_fractional_scale() {
        for device in [1.125, 1.25, 1.4, 1.75, 1.8, 2.5, 3.875] {
            let physical = (1410, 796);
            let view = hypr_ipc::clean_mode(physical.0, physical.1, device as f32);
            let l = layout(view, device, 1410.5 / device, 796.25 / device).unwrap();
            assert_eq!(l.factor, 1.0, "scale {device}, view {view:?}");
        }
    }

    #[test]
    fn below_one_scale_preserves_drawing_and_pointer_coordinates() {
        let device = 0.8;
        let l = layout((1920, 1080), device, 2400.0, 1350.0).unwrap();
        assert_eq!((l.width, l.height), (2400.0, 1350.0));
        assert_eq!(l.width / l.factor * device, 1920.0);
        assert_eq!(l.height / l.factor * device, 1080.0);
    }

    #[test]
    fn empty_inputs_have_no_layout() {
        assert!(layout((0, 0), 2.0, 100.0, 100.0).is_none());
        assert!(layout((10, 10), 2.0, 0.0, 100.0).is_none());
    }
}
