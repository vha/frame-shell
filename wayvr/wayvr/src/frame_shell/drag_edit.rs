use std::{
    f32::consts::FRAC_PI_2,
    time::{Duration, Instant},
};

use glam::{Affine3A, Vec3A};

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
    updated: Instant,
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
        || app.anchor_grabbed
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
        updated: Instant::now(),
    });
    config.pause_movement = true;
    super::toast(
        app,
        "Hold the left mouse button to move the panel. Escape cancels.".into(),
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

pub fn finish<T: Default>(
    app: &mut AppState,
    overlays: &mut OverlayWindowManager<T>,
    owner: u64,
    commit: bool,
) -> Result<(), String> {
    if !app
        .panel_drag
        .as_ref()
        .is_some_and(|drag| drag.owner == owner)
    {
        return Err("This connection does not own a panel drag.".into());
    }
    end(app, overlays, commit);
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
