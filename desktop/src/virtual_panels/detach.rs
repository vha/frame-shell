use std::time::{Duration, Instant};

use wayvr_ipc::{
    frame::{HostMessage, VirtualTargetKind},
    virtual_panel::{DetachGesture, DetachPlacement, DropEdge, normalized_size},
};

use super::{Worker, sizing::fitted_size};
use crate::compositor::{MonitorGeometry, WindowInfo, normalize_window_address};

pub(super) const POLL_INTERVAL: Duration = Duration::from_millis(60);
const ARM_DELAY: Duration = Duration::from_millis(180);
const TIMEOUT: Duration = Duration::from_secs(15);
const EDGE_WIDTH: i32 = 24;
const ARMED_EDGE_WIDTH: i32 = 36;
const MIN_DRAG: i32 = 48;

pub(super) struct Gesture {
    id: u64,
    window: WindowInfo,
    start: [i32; 2],
    deadline: Instant,
    candidate: Option<(DropEdge, Instant)>,
    preview: Option<DetachPlacement>,
}

pub(super) struct Creation {
    pub placement: DetachPlacement,
    pub position: (i32, i32),
    pub window: WindowInfo,
    pub original_workspace: i32,
    pub original_monitor: String,
    pub original_position: [i32; 2],
    pub cursor: [i32; 2],
    pub border: u32,
}

impl Worker {
    pub(super) fn detach_gesture(&mut self, gesture: DetachGesture) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        match gesture {
            DetachGesture::Begin {
                id,
                address,
                cursor,
            } => {
                if id <= self.last_detach_release
                    || self.detach.as_ref().is_some_and(|drag| id <= drag.id)
                {
                    return;
                }
                self.cancel_detach();
                if self.panels.len() >= self.capabilities.max_panels as usize
                    || self.window_panel(&address).is_some()
                {
                    return;
                }
                let address = normalize_window_address(&address);
                let Some(window) = backend.list_windows().ok().and_then(|windows| {
                    windows
                        .into_iter()
                        .find(|w| w.address == address && w.floating && !w.fullscreen && !w.pinned)
                }) else {
                    return;
                };
                if !(0..2).all(|axis| {
                    cursor[axis] >= window.pos[axis].saturating_sub(12)
                        && cursor[axis]
                            < window.pos[axis].saturating_add(window.size[axis] as i32 + 12)
                }) {
                    return;
                }
                self.detach = Some(Gesture {
                    id,
                    window,
                    start: cursor,
                    deadline: Instant::now() + TIMEOUT,
                    candidate: None,
                    preview: None,
                });
            }
            DetachGesture::Release { id, cursor } => {
                self.last_detach_release = self.last_detach_release.max(id);
                if self.detach.as_ref().is_none_or(|gesture| gesture.id != id) {
                    return;
                }
                let gesture = self.detach.take().expect("matching detach gesture");
                if gesture.preview.is_some() {
                    self.output.send(&HostMessage::DetachPreview(None));
                }
                let Some(preview) = gesture.preview.as_ref() else {
                    return;
                };
                let Some(creation) = self.detach_candidate(&gesture, cursor) else {
                    return;
                };
                if creation.placement.edge != preview.edge
                    || self.window_panel(&creation.window.address).is_some()
                {
                    return;
                }
                let target = VirtualTargetKind::Window {
                    address: creation.window.address.clone(),
                    title: creation.window.title.clone(),
                    app_id: creation.window.app_id.clone(),
                };
                self.create_panel(target, creation.placement.size, Some(creation));
            }
        }
    }

    pub(super) fn cancel_detach(&mut self) {
        if self
            .detach
            .take()
            .is_some_and(|gesture| gesture.preview.is_some())
        {
            self.output.send(&HostMessage::DetachPreview(None));
        }
    }

    pub(super) fn poll_detach(&mut self) {
        let Some(mut gesture) = self.detach.take() else {
            return;
        };
        let now = Instant::now();
        if now >= gesture.deadline {
            if gesture.preview.is_some() {
                self.output.send(&HostMessage::DetachPreview(None));
            }
            return;
        }
        if now < self.next_detach_poll {
            self.detach = Some(gesture);
            return;
        }
        self.next_detach_poll = now + POLL_INTERVAL;
        let candidate = self
            .backend
            .as_ref()
            .and_then(|backend| backend.cursor_position().ok())
            .and_then(|cursor| self.detach_candidate(&gesture, cursor));
        let edge = candidate.as_ref().map(|creation| creation.placement.edge);
        if edge != gesture.candidate.map(|(edge, _)| edge) {
            gesture.candidate = edge.map(|edge| (edge, now));
        }
        let preview = candidate.map(|creation| creation.placement).filter(|_| {
            gesture
                .candidate
                .is_some_and(|(_, since)| now.duration_since(since) >= ARM_DELAY)
        });
        if preview != gesture.preview {
            gesture.preview = preview;
            self.output
                .send(&HostMessage::DetachPreview(gesture.preview.clone()));
        }
        self.detach = Some(gesture);
    }

    fn detach_candidate(&self, gesture: &Gesture, cursor: [i32; 2]) -> Option<Creation> {
        if Instant::now() >= gesture.deadline
            || self.panels.len() >= self.capabilities.max_panels as usize
        {
            return None;
        }
        let backend = self.backend.as_ref()?;
        let monitors = backend.list_monitors().ok()?;
        let anchor = monitors
            .iter()
            .find(|m| m.name == gesture.window.monitor_name)?;
        if !anchor.contains(gesture.start)
            || !anchor.contains(cursor)
            || anchor.active_workspace_id != gesture.window.workspace_id
        {
            return None;
        }
        let armed = gesture.preview.as_ref().map(|preview| preview.edge);
        let edge = drop_edge(&monitors, anchor, gesture.start, cursor, armed)?;
        let window = backend.list_windows().ok()?.into_iter().find(|window| {
            window.address == gesture.window.address
                && window.floating
                && !window.fullscreen
                && !window.pinned
                // Hyprland can snap the window back or assign its center to another
                // output as the drag ends. Once armed, use the cursor to judge intent.
                && if armed.is_some() {
                    window.workspace_id == gesture.window.workspace_id
                        || window.monitor_name != anchor.name
                } else {
                    window.workspace_id == gesture.window.workspace_id
                        && window.monitor_name == anchor.name
                        && window.pos != gesture.window.pos
                        && window.size == gesture.window.size
                }
        })?;
        let border = backend.window_border().ok()?;
        // The new output has no reserved space and runs at scale 1.
        let mut new_monitor = anchor.clone();
        new_monitor.reserved = [0; 4];
        let fitted = fitted_size(&window, &new_monitor, border);
        let size = normalized_size(fitted);
        if (0..2).any(|axis| size[axis] < fitted[axis]) {
            return None;
        }
        let (placement, position) = place_at_edge(&monitors, anchor, edge, cursor, size)?;
        Some(Creation {
            placement,
            position,
            window,
            original_workspace: gesture.window.workspace_id,
            original_monitor: gesture.window.monitor_name.clone(),
            original_position: gesture.window.pos,
            cursor,
            border,
        })
    }
}

fn drop_edge(
    monitors: &[MonitorGeometry],
    anchor: &MonitorGeometry,
    start: [i32; 2],
    cursor: [i32; 2],
    armed: Option<DropEdge>,
) -> Option<DropEdge> {
    let (x, y) = anchor.logical_pos;
    let (w, h) = anchor.logical_size;
    [
        (
            DropEdge::Left,
            cursor[0] - x,
            start[0] - cursor[0],
            [x - 1, cursor[1]],
        ),
        (
            DropEdge::Right,
            x + w - 1 - cursor[0],
            cursor[0] - start[0],
            [x + w, cursor[1]],
        ),
        (
            DropEdge::Top,
            cursor[1] - y,
            start[1] - cursor[1],
            [cursor[0], y - 1],
        ),
        (
            DropEdge::Bottom,
            y + h - 1 - cursor[1],
            cursor[1] - start[1],
            [cursor[0], y + h],
        ),
    ]
    .into_iter()
    .filter(|&(edge, distance, motion, beyond)| {
        let width = if armed == Some(edge) {
            ARMED_EDGE_WIDTH
        } else {
            EDGE_WIDTH
        };
        distance < width
            && motion >= MIN_DRAG
            && !monitors
                .iter()
                .any(|m| m.name != anchor.name && m.contains(beyond))
    })
    .min_by_key(|&(edge, distance, _, _)| (armed != Some(edge), distance))
    .map(|(edge, _, _, _)| edge)
}

fn place_at_edge(
    monitors: &[MonitorGeometry],
    anchor: &MonitorGeometry,
    edge: DropEdge,
    cursor: [i32; 2],
    size: [u32; 2],
) -> Option<(DetachPlacement, (i32, i32))> {
    let origin = [anchor.logical_pos.0, anchor.logical_pos.1];
    let anchor_size = [anchor.logical_size.0, anchor.logical_size.1];
    let extent = size.map(|v| v as i32);
    let axis = usize::from(matches!(edge, DropEdge::Top | DropEdge::Bottom));
    let cross = 1 - axis;
    let mut pos = origin;
    pos[axis] += if matches!(edge, DropEdge::Left | DropEdge::Top) {
        -extent[axis]
    } else {
        anchor_size[axis]
    };
    let min_overlap = 64.min(anchor_size[cross]).min(extent[cross]);
    let min = origin[cross] + min_overlap - extent[cross];
    let max = origin[cross] + anchor_size[cross] - min_overlap;
    let ideal = (cursor[cross] - extent[cross] / 2).clamp(min, max);
    let blockers: Vec<_> = monitors
        .iter()
        .filter_map(|m| {
            let start = [m.logical_pos.0, m.logical_pos.1];
            let length = [m.logical_size.0, m.logical_size.1];
            (pos[axis] < start[axis] + length[axis] && start[axis] < pos[axis] + extent[axis])
                .then_some((start[cross], start[cross] + length[cross]))
        })
        .collect();
    pos[cross] = [ideal, min, max]
        .into_iter()
        .chain(
            blockers
                .iter()
                .flat_map(|&(start, end)| [start - extent[cross], end]),
        )
        .filter(|&p| {
            (min..=max).contains(&p)
                && blockers
                    .iter()
                    .all(|&(start, end)| p >= end || p + extent[cross] <= start)
        })
        .min_by_key(|&p| p.abs_diff(ideal))?;
    let hint = DetachPlacement {
        anchor: anchor.name.clone(),
        anchor_size,
        edge,
        size,
        offset: pos[cross] - origin[cross] + extent[cross] / 2 - anchor_size[cross] / 2,
    };
    Some((hint, (pos[0], pos[1])))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, x: i32, y: i32, w: i32, h: i32) -> MonitorGeometry {
        MonitorGeometry {
            id: 0,
            name: name.into(),
            width: w as u32,
            height: h as u32,
            logical_pos: (x, y),
            logical_size: (w, h),
            scale: 1.0,
            reserved: [0; 4],
            focused: false,
            active_workspace_id: 1,
            active_workspace_name: "1".into(),
        }
    }

    #[test]
    fn shared_edge_and_pointer_pullback_cancel_the_drop() {
        let monitors = [
            monitor("DP-1", -1920, 0, 1920, 1080),
            monitor("DP-2", 0, 0, 1920, 1080),
        ];
        let start = [-1000, 500];
        assert_eq!(
            drop_edge(
                &monitors,
                &monitors[0],
                start,
                [-1, 500],
                Some(DropEdge::Right)
            ),
            None
        );
        assert_eq!(
            drop_edge(&monitors, &monitors[0], start, [-1919, 500], None),
            Some(DropEdge::Left)
        );
        assert_eq!(
            drop_edge(
                &monitors,
                &monitors[0],
                start,
                [-1850, 500],
                Some(DropEdge::Left)
            ),
            None
        );
        assert_eq!(
            drop_edge(&monitors, &monitors[0], start, [-1890, 500], None),
            None
        );
        assert_eq!(
            drop_edge(
                &monitors,
                &monitors[0],
                start,
                [-1890, 500],
                Some(DropEdge::Left)
            ),
            Some(DropEdge::Left)
        );
    }

    #[test]
    fn placement_slides_along_the_edge_to_clear_other_outputs() {
        let monitors = [
            monitor("DP-1", -1920, 0, 1920, 1080),
            monitor("DP-2", -2800, 0, 880, 400),
        ];
        let (_, pos) = place_at_edge(
            &monitors,
            &monitors[0],
            DropEdge::Left,
            [-1919, 500],
            [800, 480],
        )
        .unwrap();
        assert_eq!(pos, (-2720, 400));
    }
}
