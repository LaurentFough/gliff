//! Pick the compositor's rendering GPU instead of assuming renderD128.
use hypr_wl::Target;
use std::path::{Path, PathBuf};
use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::{delegate_noop, Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_dmabuf_feedback_v1::{self as feedback, ZwpLinuxDmabufFeedbackV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};

#[derive(Default)]
struct Probe {
    device: Option<u64>,
}

pub fn compositor_render_node(target: &Target) -> Option<PathBuf> {
    let (_conn, globals, mut queue) = hypr_wl::init::<Probe>(target).ok()?;
    let qh = queue.handle();
    let dmabuf: ZwpLinuxDmabufV1 = globals.bind(&qh, 4..=5, ()).ok()?;
    let feedback = dmabuf.get_default_feedback(&qh, ());
    let mut state = Probe::default();
    queue.roundtrip(&mut state).ok()?;
    feedback.destroy();
    dmabuf.destroy();
    render_node_for_device(Path::new("/sys/dev/char"), state.device?)
}

fn render_node_for_device(sysfs: &Path, device: u64) -> Option<PathBuf> {
    let path = sysfs.join(format!(
        "{}:{}",
        nix::sys::stat::major(device),
        nix::sys::stat::minor(device)
    ));
    // The feedback device may name either the primary or render DRM node.
    let drm = path.join("device/drm");
    std::fs::read_dir(drm)
        .ok()?
        .filter_map(Result::ok)
        .find_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            name.strip_prefix("renderD")?.parse::<u32>().ok()?;
            Some(Path::new("/dev/dri").join(name))
        })
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &ZwpLinuxDmabufFeedbackV1,
        event: feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let feedback::Event::MainDevice { device } = event {
            state.device = device.try_into().ok().map(u64::from_ne_bytes);
        }
    }
}
impl Dispatch<WlRegistry, GlobalListContents> for Probe {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: wayland_client::protocol::wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Probe: ignore ZwpLinuxDmabufV1);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_render_node_does_not_require_a_compositor() {
        let target = Target {
            display: Some("/nonexistent/gliff-test-wayland".into()),
            instance: None,
        };
        let explicit = Path::new("/dev/dri/renderD129");
        assert_eq!(
            crate::render_node_for_target(Some(explicit), &target),
            explicit
        );
    }

    #[test]
    fn resolves_primary_feedback_to_matching_render_node() {
        let dir = std::env::temp_dir().join(format!("gliff-drm-device-{}", std::process::id()));
        let drm = dir.join("226:1/device/drm");
        std::fs::create_dir_all(drm.join("card1")).unwrap();
        std::fs::create_dir_all(drm.join("renderD129")).unwrap();
        assert_eq!(
            render_node_for_device(&dir, nix::sys::stat::makedev(226, 1)),
            Some(PathBuf::from("/dev/dri/renderD129"))
        );
        assert_eq!(
            render_node_for_device(&dir, nix::sys::stat::makedev(226, 0)),
            None
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
