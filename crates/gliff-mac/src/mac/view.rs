//! `GliffView`: the NSView that shows the remote picture and turns pointer
//! events into protocol messages. Keyboard events are taken earlier, by a
//! local event monitor (see `mac::install_key_monitor`), so Command
//! shortcuts and Command key-ups reach the remote too.
//!
//! The view is layer-backed: the picture is a `CALayer` sublayer whose
//! `contents` is a zero-copy `CGImage` of the decoded frame, so scaling to
//! the window is done by Core Animation on the GPU.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSEvent, NSResponder, NSView};
use objc2_foundation::{NSObject, NSPoint, NSRect};

use super::ui;

//= Version of the view's event handling.
pub const VIEW_VERSION: &str = "0.1.0";

define_class!(
    // SAFETY: NSView may be subclassed; no overridden method changes its
    // memory-management contract, and `Drop` is not implemented.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GliffView"]
    pub struct GliffView;

    impl GliffView {
        //= Top-left origin, matching the remote's coordinate system.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        //= A click on an inactive window reaches the remote too.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            ui::pointer_moved(self.point(event));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            ui::pointer_moved(self.point(event));
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            ui::pointer_moved(self.point(event));
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            ui::pointer_moved(self.point(event));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.button(event, true);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.button(event, false);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.button(event, true);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.button(event, false);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.button(event, true);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.button(event, false);
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            // SAFETY: plain NSEvent property getters on a scroll event.
            let (dx, dy, precise, phase, momentum): (f64, f64, bool, usize, usize) = unsafe {
                (
                    msg_send![event, scrollingDeltaX],
                    msg_send![event, scrollingDeltaY],
                    msg_send![event, hasPreciseScrollingDeltas],
                    msg_send![event, phase],
                    msg_send![event, momentumPhase],
                )
            };
            ui::scrolled(dx, dy, precise, phase, momentum);
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, _event: &NSEvent) {
            ui::pointer_entered();
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            ui::pointer_left();
        }

        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {
            ui::apply_cursor();
        }
    }
);

impl GliffView {
    /// A layer-backed view with a tracking area that follows its bounds.
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: designated initialiser of NSView.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // SAFETY: standard NSView/NSTrackingArea setup on the main thread.
        unsafe {
            let _: () = msg_send![&*this, setWantsLayer: true];
            //= NSTrackingMouseEnteredAndExited | MouseMoved | CursorUpdate |
            //= ActiveInKeyWindow | InVisibleRect (follows the bounds).
            let options: usize = 0x01 | 0x02 | 0x04 | 0x20 | 0x200;
            let alloc: objc2::rc::Allocated<AnyObject> =
                msg_send![objc2::class!(NSTrackingArea), alloc];
            let area: Retained<AnyObject> = msg_send![
                alloc,
                initWithRect: NSRect::ZERO,
                options: options,
                owner: &*this,
                userInfo: std::ptr::null::<AnyObject>()
            ];
            let _: () = msg_send![&*this, addTrackingArea: &*area];
        }
        this
    }

    /// The event's location in this view (points, top-left origin).
    fn point(&self, event: &NSEvent) -> (f64, f64) {
        // SAFETY: NSEvent/NSView geometry getters on the main thread.
        let p: NSPoint = unsafe {
            let in_window: NSPoint = msg_send![event, locationInWindow];
            msg_send![self, convertPoint: in_window, fromView: std::ptr::null::<AnyObject>()]
        };
        (p.x, p.y)
    }

    fn button(&self, event: &NSEvent, pressed: bool) {
        // SAFETY: NSEvent getter.
        let number: isize = unsafe { msg_send![event, buttonNumber] };
        if pressed {
            // SAFETY: make this view first responder of its own window.
            unsafe {
                let window: Option<Retained<AnyObject>> = msg_send![self, window];
                if let Some(w) = window {
                    let _: bool = msg_send![&*w, makeFirstResponder: self];
                }
            }
        }
        ui::pointer_moved(self.point(event));
        ui::button(number as i64, pressed);
    }

    /// Bounds size in points.
    pub fn size(&self) -> (f64, f64) {
        // SAFETY: NSView getter.
        let b: NSRect = unsafe { msg_send![self, bounds] };
        (b.size.width, b.size.height)
    }

    /// The backing scale factor of the view's window (1.0 off-window).
    pub fn device_scale(&self) -> f64 {
        // SAFETY: NSView/NSWindow getters.
        unsafe {
            let window: Option<Retained<AnyObject>> = msg_send![self, window];
            match window {
                Some(w) => msg_send![&*w, backingScaleFactor],
                None => 1.0,
            }
        }
    }
}
