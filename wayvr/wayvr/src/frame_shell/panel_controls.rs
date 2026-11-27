use std::sync::atomic::Ordering;

use glam::Vec3A;
use wayvr_ipc::{
    frame::{ShellMessage, VirtualTargetKind},
    virtual_panel::{
        Action, Direction, HostPanels, Panel, Request, Response, SIZE_REQUIREMENTS, Target,
        valid_size,
    },
};

use crate::{
    backend::task::{OverlayTask, ToggleMode},
    frame_host,
    state::AppState,
    windowing::{OverlaySelector, manager::OverlayWindowManager},
};

use super::{EMBEDDED, layout_changed, virtual_overlay_name};

pub(crate) fn handle_request<T: Default>(
    app: &mut AppState,
    overlays: &mut OverlayWindowManager<T>,
    connection: u64,
    request: Request,
) -> Result<Response, String> {
    if !EMBEDDED.load(Ordering::Relaxed) {
        return Err("Virtual panel controls are available in Frame Shell.".into());
    }
    let request = match request {
        Request::BeginDragAtOutput { output } => {
            let source = app
                .ipc_server
                .remote_source(connection)
                .ok_or("Select a connected PC's panel in the picker first.")?;
            let surface = frame_host::virtual_state(source)
                .and_then(|state| {
                    state
                        .panels
                        .into_iter()
                        .find(|panel| panel.output_name == output)
                })
                .map(|panel| panel.surface)
                .ok_or("No virtual panel under the cursor. Select a panel in the picker first.")?;
            Request::BeginDrag { source, surface }
        }
        other => other,
    };
    let source = match request {
        Request::List => return Ok(Response::Hosts(list_hosts(overlays))),
        Request::DragMotion {
            translation,
            scale_steps,
        } => {
            super::drag_edit::motion(app, overlays, connection, translation, scale_steps)?;
            return Ok(Response::Accepted);
        }
        Request::EndDrag { commit } => {
            super::drag_edit::finish(app, overlays, connection, commit)?;
            return Ok(Response::Accepted);
        }
        Request::BeginDrag { source, .. } => source,
        Request::Refresh { source }
        | Request::Create { source, .. }
        | Request::Control { source, .. } => source,
        Request::BeginDragAtOutput { .. } => unreachable!(),
    };
    let state = frame_host::virtual_state(source).ok_or_else(|| {
        format!("PC {source} is disconnected or does not report virtual panel support.")
    })?;
    if !state.capabilities.supported {
        return Err(state
            .capabilities
            .reason
            .unwrap_or_else(|| "Virtual panels are unavailable on this PC.".into()));
    }

    let message = match request {
        Request::List
        | Request::BeginDragAtOutput { .. }
        | Request::DragMotion { .. }
        | Request::EndDrag { .. } => unreachable!(),
        Request::BeginDrag { surface, .. } => {
            let info = state
                .panels
                .iter()
                .find(|p| p.surface == surface)
                .ok_or("That virtual panel no longer exists.")?;
            let name = virtual_overlay_name(source, surface, &info.output_name);
            let id = overlays
                .lookup(&name)
                .ok_or("The panel has not appeared in the headset yet.")?;
            super::drag_edit::begin(app, overlays, id, source, surface, connection)?;
            return Ok(Response::Accepted);
        }
        Request::Refresh { .. } => ShellMessage::RefreshVirtualTargets,
        Request::Create { target, size, .. } => {
            if !valid_size(size) {
                return Err(SIZE_REQUIREMENTS.into());
            }
            let targets = frame_host::virtual_targets(source).unwrap_or_default();
            let source = match target {
                Target::Blank => VirtualTargetKind::Blank {
                    width: size[0],
                    height: size[1],
                },
                Target::Window(address) => {
                    let window = targets
                        .windows
                        .into_iter()
                        .find(|w| w.address == address)
                        .ok_or_else(|| {
                            "Window is no longer listed. Refresh the PC's targets.".to_owned()
                        })?;
                    VirtualTargetKind::Window {
                        address: window.address,
                        title: window.title,
                        app_id: window.app_id,
                    }
                }
                Target::Workspace(id) => {
                    let workspace = targets
                        .workspaces
                        .into_iter()
                        .find(|w| w.id == id)
                        .ok_or_else(|| {
                            "Workspace is no longer listed. Refresh the PC's targets.".to_owned()
                        })?;
                    VirtualTargetKind::Workspace {
                        id,
                        name: workspace.name,
                    }
                }
            };
            ShellMessage::CreateVirtualPanel { source, size }
        }
        Request::Control {
            surface, action, ..
        } => {
            if app
                .panel_drag
                .as_ref()
                .is_some_and(|drag| drag.targets(source, surface))
            {
                return Err("Finish the mouse drag before changing this panel.".into());
            }
            let info = state
                .panels
                .iter()
                .find(|panel| panel.surface == surface)
                .ok_or_else(|| "That virtual panel no longer exists.".to_owned())?;
            match action {
                Action::Close => ShellMessage::CloseVirtualPanel { surface },
                Action::Focus => {
                    if !state.capabilities.focus_panel {
                        return Err(
                            "This PC server does not support focusing virtual panels.".into()
                        );
                    }
                    ShellMessage::FocusVirtualPanel { surface }
                }
                Action::Sizing(sizing) => {
                    if !state.capabilities.resize_panel {
                        return Err(
                            "This PC server does not support changing panel resolution.".into()
                        );
                    }
                    if let wayvr_ipc::virtual_panel::Sizing::Resolution(size) = sizing
                        && !valid_size(size)
                    {
                        return Err(SIZE_REQUIREMENTS.into());
                    }
                    ShellMessage::SetVirtualPanelSizing { surface, sizing }
                }
                _ => {
                    let name = virtual_overlay_name(source, surface, &info.output_name);
                    let id = overlays.lookup(&name).ok_or_else(|| {
                        "The panel has not appeared in the headset yet.".to_owned()
                    })?;
                    edit_overlay(app, overlays, id, action)?;
                    return Ok(Response::Accepted);
                }
            }
        }
    };

    if !frame_host::send(source, &message) {
        return Err(format!("Could not send the request to PC {source}."));
    }
    Ok(Response::Accepted)
}

fn list_hosts<T: Default>(overlays: &OverlayWindowManager<T>) -> Vec<HostPanels> {
    super::virtual_supported_sources()
        .into_iter()
        .filter_map(|(source, name, max_panels, _)| {
            let state = frame_host::virtual_state(source)?;
            let panels = state
                .panels
                .into_iter()
                .map(|info| {
                    let name = virtual_overlay_name(source, info.surface, &info.output_name);
                    let visible = overlays.iter().any(|(_, overlay)| {
                        overlay.config.name == name && overlay.config.is_active()
                    });
                    Panel { info, visible }
                })
                .collect();
            Some(HostPanels {
                source,
                name,
                max_panels,
                resize_panel: state.capabilities.resize_panel,
                drag_panel: true,
                panels,
                targets: frame_host::virtual_targets(source).unwrap_or_default(),
            })
        })
        .collect()
}

fn edit_overlay<T: Default>(
    app: &mut AppState,
    overlays: &mut OverlayWindowManager<T>,
    id: crate::windowing::OverlayID,
    action: Action,
) -> Result<(), String> {
    if app.anchor_grabbed || app.input_state.is_grabbing_overlay(id) {
        return Err("Release the controller grab before editing this panel.".into());
    }
    match action {
        Action::Show | Action::Hide | Action::Reset => {
            let selector = OverlaySelector::Id(id);
            let task = match action {
                Action::Show => OverlayTask::ToggleOverlay(selector, ToggleMode::EnsureOn),
                Action::Hide => OverlayTask::ToggleOverlay(selector, ToggleMode::EnsureOff),
                Action::Reset => OverlayTask::ResetOverlay(selector),
                _ => unreachable!(),
            };
            overlays
                .handle_task(app, task)
                .map_err(|err| format!("{err:#}"))?;
            if matches!(action, Action::Reset) {
                layout_changed();
            }
        }
        Action::Nudge { direction, fine } => {
            let overlay = overlays
                .mut_by_id(id)
                .ok_or_else(|| "The panel has closed.".to_owned())?;
            let state = overlay
                .config
                .active_state
                .as_mut()
                .ok_or_else(|| "Show the virtual panel before moving it.".to_owned())?;
            nudge(&mut state.transform, direction, fine);
            overlay.config.dirty = true;
            layout_changed();
        }
        Action::Close | Action::Focus | Action::Sizing(_) => unreachable!("host action"),
    }
    Ok(())
}

fn nudge(transform: &mut glam::Affine3A, direction: Direction, fine: bool) {
    let step = if fine { 0.01 } else { 0.05 };
    let right = transform.x_axis.normalize_or_zero();
    let up = transform.y_axis.normalize_or_zero();
    let toward_viewer = right.cross(up).normalize_or_zero();
    let delta = match direction {
        Direction::Left => -right,
        Direction::Right => right,
        Direction::Up => up,
        Direction::Down => -up,
        Direction::Nearer => toward_viewer,
        Direction::Farther => -toward_viewer,
        Direction::Larger | Direction::Smaller => {
            let scale = transform.x_axis.length();
            let factor = if fine { 1.02_f32 } else { 1.1_f32 };
            let factor = if matches!(direction, Direction::Smaller) {
                factor.recip()
            } else {
                factor
            };
            if scale > f32::EPSILON {
                transform.matrix3 = transform
                    .matrix3
                    .mul_scalar((scale * factor).clamp(0.1, 20.0) / scale);
            }
            Vec3A::ZERO
        }
    };
    transform.translation += delta * step;
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::{Affine3A, Quat, Vec3};

    #[test]
    fn nudge_uses_panel_axes_and_fine_step() {
        let mut transform = Affine3A::from_scale_rotation_translation(
            Vec3::splat(2.0),
            Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            Vec3::new(1.0, 2.0, -3.0),
        );
        let axes = transform.matrix3;
        nudge(&mut transform, Direction::Right, true);
        nudge(&mut transform, Direction::Nearer, false);
        assert!(
            transform
                .translation
                .abs_diff_eq(Vec3A::new(1.0, 2.01, -2.95), 1e-5)
        );
        assert_eq!(transform.matrix3, axes);
    }

    #[test]
    fn scale_nudge_preserves_position_and_clamps_at_limit() {
        let position = Vec3::new(1.0, 2.0, -3.0);
        let mut transform =
            Affine3A::from_scale_rotation_translation(Vec3::splat(19.0), Quat::IDENTITY, position);
        nudge(&mut transform, Direction::Larger, false);
        assert!((transform.x_axis.length() - 20.0).abs() < 1e-5);
        nudge(&mut transform, Direction::Smaller, false);
        assert!(transform.x_axis.length() < 20.0);
        assert_eq!(transform.translation, Vec3A::from(position));
    }
}
