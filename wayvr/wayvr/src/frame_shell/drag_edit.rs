use std::{
    f32::consts::FRAC_PI_2,
    time::{Duration, Instant},
};

use glam::{Affine3A, Vec2, Vec3A};
use wayvr_ipc::{
    frame::ShellMessage,
    virtual_panel::{MAX_SIZE, MIN_SIZE, Sizing, normalized_size},
};

use crate::{
    frame_host,
    state::AppState,
    windowing::{
        OverlayID,
        manager::OverlayWindowManager,
        window::{realign, save_transform, scalar_scale},
    },
};

const TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Drag {
    owner: u64,
    source: u32,
    surface: u32,
    overlay: OverlayID,
    original: Affine3A,
    orbit: Orbit,
    resize: Option<Resize>,
    updated: Instant,
}

struct Resize {
    original: [u32; 2],
    pixels: [f64; 2],
}

impl Resize {
    fn new(original: [u32; 2]) -> Self {
        Self {
            original,
            pixels: original.map(f64::from),
        }
    }

    fn apply(&mut self, delta: [f32; 2]) {
        for (axis, pixels) in self.pixels.iter_mut().enumerate() {
            *pixels = (*pixels + f64::from(delta[axis]))
                .clamp(f64::from(MIN_SIZE[axis]), f64::from(MAX_SIZE[axis]));
        }
    }

    fn proposal(&self) -> Option<[u32; 2]> {
        let size = normalized_size(self.pixels.map(|pixels| (pixels * 0.5).round() as u32 * 2));
        (size != self.original).then_some(size)
    }
}

impl Drag {
    pub fn targets(&self, source: u32, surface: u32) -> bool {
        (self.source, self.surface) == (source, surface)
    }
}

// Keep the drag centered on the headset's starting position so moving your
// head during the drag doesn't translate the panel.
struct Orbit {
    center: Vec3A,
    distance: f32,
    max_distance: f32,
    yaw: f32,
    elevation: f32,
}

impl Orbit {
    fn new(center: Vec3A, position: Vec3A) -> Self {
        let offset = position - center;
        let distance = offset.length();
        Self {
            center,
            distance,
            max_distance: distance + 10.0,
            yaw: offset.x.atan2(-offset.z),
            elevation: offset.y.atan2(offset.x.hypot(offset.z)),
        }
    }

    fn move_by(&mut self, [dx, dy, dz]: [f32; 3]) -> Vec3A {
        const MIN_DISTANCE: f32 = 0.1;
        let horizontal_radius = self.distance * self.elevation.cos();
        self.yaw += dx / horizontal_radius.max(MIN_DISTANCE);
        self.elevation =
            (self.elevation + dy / self.distance.max(MIN_DISTANCE)).clamp(-FRAC_PI_2, FRAC_PI_2);
        if dz != 0.0 {
            // The scroll direction is positive toward the headset.
            self.distance = (self.distance - dz).clamp(MIN_DISTANCE, self.max_distance);
        }
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_elevation, cos_elevation) = self.elevation.sin_cos();
        self.center
            + self.distance
                * Vec3A::new(
                    sin_yaw * cos_elevation,
                    sin_elevation,
                    -cos_yaw * cos_elevation,
                )
    }
}

pub fn begin<T: Default>(
    app: &mut AppState,
    overlays: &mut OverlayWindowManager<T>,
    overlay: OverlayID,
    source: u32,
    surface: u32,
    owner: u64,
) -> Result<(), String> {
    if app.panel_drag.is_some()
        || app.input_state.is_grabbing_anchor()
        || app.input_state.is_grabbing_overlay(overlay)
    {
        return Err("Another panel grab is in progress.".into());
    }
    let config = &mut overlays
        .mut_by_id(overlay)
        .ok_or("The panel has closed.")?
        .config;
    let state = config
        .active_state
        .as_ref()
        .filter(|state| state.alpha > 0.05 && state.grabbable)
        .ok_or("Show the movable panel before dragging it.")?;
    app.panel_drag = Some(Drag {
        owner,
        source,
        surface,
        overlay,
        original: state.transform,
        orbit: Orbit::new(app.input_state.hmd.translation, state.transform.translation),
        resize: None,
        updated: Instant::now(),
    });
    config.pause_movement = true;
    super::toast(
        app,
        "Left-drag to move. Right-drag to resize. Escape cancels.".into(),
    );
    Ok(())
}

pub fn motion<T: Default>(
    app: &mut AppState,
    overlays: &mut OverlayWindowManager<T>,
    owner: u64,
    translation: [f32; 3],
    scale_steps: f32,
) -> Result<(), String> {
    let drag = app
        .panel_drag
        .as_mut()
        .filter(|drag| drag.owner == owner)
        .ok_or("This connection does not own a panel drag.")?;
    if drag.resize.is_some() {
        return Err("Finish resizing before moving the panel.".into());
    }
    if translation.iter().any(|v| !v.is_finite() || v.abs() > 0.5)
        || !scale_steps.is_finite()
        || scale_steps.abs() > 6.0
    {
        return Err("Invalid panel drag motion.".into());
    }
    let overlay = overlays
        .mut_by_id(drag.overlay)
        .ok_or("The panel has closed.")?;
    let state = overlay
        .config
        .active_state
        .as_mut()
        .filter(|state| state.alpha > 0.05)
        .ok_or("The panel is no longer visible.")?;
    drag.updated = Instant::now();
    if translation != [0.0; 3] || scale_steps != 0.0 {
        let transform = &mut state.transform;
        if translation != [0.0; 3] {
            transform.translation = drag.orbit.move_by(translation);
        }
        let mut scale = scalar_scale(transform);
        if scale > f32::EPSILON && scale_steps != 0.0 {
            let target = (scale * 1.1_f32.powf(scale_steps)).clamp(0.1, 20.0);
            transform.matrix3 = transform.matrix3.mul_scalar(target / scale);
            scale = target;
        }
        realign(
            transform,
            &app.input_state.hmd,
            scale,
            app.session.config.snap_angle_deg,
        );
        overlay.config.dirty = true;
    }
    Ok(())
}

pub fn resize(app: &mut AppState, owner: u64, delta: [f32; 2]) -> Result<(), String> {
    let drag = app
        .panel_drag
        .as_mut()
        .filter(|drag| drag.owner == owner)
        .ok_or("This connection does not own a panel drag.")?;
    if delta.iter().any(|value| !value.is_finite()) {
        return Err("Invalid panel resize motion.".into());
    }
    if drag.resize.is_none() {
        let state = frame_host::virtual_state(drag.source)
            .filter(|state| state.capabilities.resize_panel)
            .ok_or("This PC does not support changing panel resolution.")?;
        let panel = state
            .panels
            .iter()
            .find(|panel| panel.surface == drag.surface)
            .ok_or("The panel has closed.")?;
        drag.resize = Some(Resize::new([panel.width, panel.height]));
    }
    drag.resize
        .as_mut()
        .expect("resize initialized")
        .apply(delta);
    drag.updated = Instant::now();
    Ok(())
}

pub fn resize_preview<T>(
    app: &AppState,
    overlays: &OverlayWindowManager<T>,
) -> Option<(Affine3A, Vec2)> {
    let drag = app.panel_drag.as_ref()?;
    let resize = drag.resize.as_ref()?;
    let [width, height] = resize.proposal()?;
    let (_, overlay) = overlays.iter().find(|(id, _)| *id == drag.overlay)?;
    let state = overlay
        .config
        .active_state
        .as_ref()
        .filter(|state| state.alpha > 0.05)?;
    let pose = overlay.config.surface_transform(state.transform);
    let density = pose.x_axis.length() / resize.original[0].max(resize.original[1]).max(1) as f32;
    // Put the outline just in front of the stream to avoid z-fighting.
    let mut pose = pose;
    pose.translation += pose.z_axis.normalize_or_zero() * 0.01;
    Some((pose, Vec2::new(width as f32, height as f32) * density))
}

pub fn finish<T: Default>(
    app: &mut AppState,
    overlays: &mut OverlayWindowManager<T>,
    owner: u64,
    commit: bool,
) -> Result<(), String> {
    let drag = app
        .panel_drag
        .as_ref()
        .filter(|drag| drag.owner == owner)
        .ok_or("This connection does not own a panel drag.")?;
    if let Some(resize) = &drag.resize {
        let target = commit
            .then(|| resize.proposal())
            .flatten()
            .map(|size| (drag.source, drag.surface, size));
        end(app, overlays, false);
        if let Some((source, surface, size)) = target
            && !frame_host::send(
                source,
                &ShellMessage::SetVirtualPanelSizing {
                    surface,
                    sizing: Sizing::Resolution(size),
                },
            )
        {
            return Err("The PC disconnected before resizing the panel.".into());
        }
    } else {
        end(app, overlays, commit);
    }
    Ok(())
}

fn end<T>(app: &mut AppState, overlays: &mut OverlayWindowManager<T>, commit: bool) {
    let Some(drag) = app.panel_drag.take() else {
        return;
    };
    if let Some(overlay) = overlays.mut_by_id(drag.overlay) {
        overlay.config.pause_movement = false;
        if commit && let Some(state) = &mut overlay.config.active_state {
            save_transform(state, app);
        }
    }
    if commit {
        super::layout_changed();
    } else {
        overlays.restore_overlay_transform(drag.overlay, drag.original);
    }
}

pub(crate) fn cancel<T>(app: &mut AppState, overlays: &mut OverlayWindowManager<T>) {
    end(app, overlays, false);
}

pub fn expire<T: Default>(app: &mut AppState, overlays: &mut OverlayWindowManager<T>) {
    if app.panel_drag.as_ref().is_some_and(|drag| {
        drag.updated.elapsed() >= TIMEOUT
            || !app.ipc_server.has_connection(drag.owner)
            || !frame_host::is_connected(drag.source)
            || overlays.mut_by_id(drag.overlay).is_none_or(|o| {
                o.config
                    .active_state
                    .as_ref()
                    .is_none_or(|s| s.alpha <= 0.05)
            })
    }) {
        end(app, overlays, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_keeps_even_dimensions_and_can_reverse_at_a_limit() {
        let mut resize = Resize::new([1280, 720]);
        assert_eq!(resize.proposal(), None);
        resize.apply([120.0, 0.0]);
        assert_eq!(resize.proposal(), Some([1400, 720]));
        resize.apply([-120.0, 80.0]);
        assert_eq!(resize.proposal(), Some([1280, 800]));
        resize.apply([5000.0, -1000.0]);
        assert_eq!(resize.proposal(), Some([MAX_SIZE[0], MIN_SIZE[1]]));
        resize.apply([-2.0, 2.0]);
        assert_eq!(resize.proposal(), Some([MAX_SIZE[0] - 2, MIN_SIZE[1] + 2]));
    }

    #[test]
    fn long_drag_across_headset_keeps_distance_and_height() {
        let headset = Vec3A::new(0.2, 1.6, 0.3);
        let start = headset + Vec3A::new(-1.3, -0.2, -1.1);
        let distance = start.distance(headset);
        let mut orbit = Orbit::new(headset, start);
        let mut end = start;
        for _ in 0..8 {
            end = orbit.move_by([0.35, 0.0, 0.0]);
            assert!((end.distance(headset) - distance).abs() < 1e-5);
            assert!((end.y - start.y).abs() < 1e-5);
        }
        assert!(end.x > headset.x + 1.0);
        assert!(end.z < headset.z);
    }

    #[test]
    fn scroll_changes_distance_and_vertical_drag_keeps_it() {
        let mut orbit = Orbit::new(Vec3A::ZERO, Vec3A::new(0.0, 0.0, -2.0));
        let raised = orbit.move_by([0.0, 0.4, 0.0]);
        assert!(raised.y > 0.0);
        assert!((raised.length() - 2.0).abs() < 1e-5);
        let nearer = orbit.move_by([0.0, 0.0, 0.5]);
        assert!(nearer.abs_diff_eq(raised * 0.75, 1e-5));
        let farther = orbit.move_by([0.0, 0.0, -0.5]);
        assert!(farther.abs_diff_eq(raised, 1e-5));
    }
}
