use std::time::{Duration, Instant};

use glam::Affine3A;

use crate::{
    frame_host,
    state::AppState,
    windowing::{OverlayID, manager::OverlayWindowManager},
};

const TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Drag {
    owner: u64,
    source: u32,
    surface: u32,
    overlay: OverlayID,
    original: Affine3A,
    updated: Instant,
}

impl Drag {
    pub fn targets(&self, source: u32, surface: u32) -> bool {
        (self.source, self.surface) == (source, surface)
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
    let state = overlays
        .mut_by_id(overlay)
        .and_then(|o| o.config.active_state.as_ref())
        .filter(|state| state.alpha > 0.05 && state.grabbable)
        .ok_or("Show the movable panel before dragging it.")?;
    app.panel_drag = Some(Drag {
        owner,
        source,
        surface,
        overlay,
        original: state.transform,
        updated: Instant::now(),
    });
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
        apply(
            &mut state.transform,
            drag.original,
            translation,
            scale_steps,
        );
        overlay.config.dirty = true;
    }
    Ok(())
}

fn apply(transform: &mut Affine3A, original: Affine3A, [dx, dy, dz]: [f32; 3], scale_steps: f32) {
    let right = original.x_axis.normalize_or_zero();
    let up = original.y_axis.normalize_or_zero();
    let normal = right.cross(up).normalize_or_zero();
    let offset =
        (transform.translation - original.translation + dx * right + dy * up + dz * normal)
            .clamp_length_max(10.0);
    transform.translation = original.translation + offset;
    let scale = transform.x_axis.length();
    if scale > f32::EPSILON && scale_steps != 0.0 {
        let target = (scale * 1.1_f32.powf(scale_steps)).clamp(0.1, 20.0);
        transform.matrix3 = transform.matrix3.mul_scalar(target / scale);
    }
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
