use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use wayvr_ipc::virtual_panel::{SIZE_REQUIREMENTS, Sizing, valid_size};

use super::{ActivePanel, PanelOwnership, Worker, preserve_cursor, to_output_geos};
use crate::{
    capture::is_virtual_output_name,
    compositor::{MonitorGeometry, WindowInfo, wlr_output::OutputConfigUpdate},
    lock,
};

const POLL_INTERVAL: Duration = Duration::from_millis(150);
const FIT_DEBOUNCE: Duration = Duration::from_millis(450);
const STREAM_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct FitWindow {
    pub enabled: bool,
    pub status: Option<String>,
    observed: Option<([u32; 2], Instant)>,
    attempted: Option<[u32; 2]>,
    settle_until: Option<Instant>,
}

impl Default for FitWindow {
    fn default() -> Self {
        Self {
            enabled: true,
            status: None,
            observed: None,
            attempted: None,
            settle_until: None,
        }
    }
}

impl Worker {
    pub(super) fn fit_poll_delay(&self) -> Duration {
        if self
            .panels
            .values()
            .any(|panel| panel.fit.as_ref().is_some_and(|fit| fit.enabled))
        {
            self.next_fit_poll.saturating_duration_since(Instant::now())
        } else {
            Duration::from_secs(1)
        }
    }

    pub(super) fn set_sizing(&mut self, surface: u32, sizing: Sizing) {
        if let Err(err) = self.apply_sizing(surface, sizing) {
            self.output
                .send(&wayvr_ipc::frame::HostMessage::Error(format!(
                    "Could not set virtual panel sizing: {err:#}"
                )));
        }
        self.broadcast_state();
    }

    fn apply_sizing(&mut self, surface: u32, sizing: Sizing) -> Result<()> {
        match sizing {
            Sizing::Resolution(size) => {
                self.resize_panel(surface, size, None)?;
                if let Some(fit) = self
                    .panels
                    .get_mut(&surface)
                    .and_then(|panel| panel.fit.as_mut())
                {
                    fit.enabled = false;
                    fit.status = None;
                }
            }
            Sizing::FitToWindow(enabled) => {
                let fit = self
                    .panels
                    .get_mut(&surface)
                    .and_then(|panel| panel.fit.as_mut())
                    .context("Fit to window is available only for a window share.")?;
                *fit = FitWindow {
                    enabled,
                    ..FitWindow::default()
                };
                self.next_fit_poll = Instant::now();
            }
        }
        Ok(())
    }

    fn visible(&self, surface: u32) -> bool {
        lock(&self.shared.visibility)
            .get(&surface)
            .copied()
            .unwrap_or(true)
    }

    pub(super) fn poll_window_fit(&mut self) {
        let now = Instant::now();
        if now < self.next_fit_poll {
            return;
        }
        self.next_fit_poll = now + POLL_INTERVAL;
        let active: Vec<_> = self
            .panels
            .iter()
            .filter_map(|(&id, panel)| {
                (self.visible(id) && panel.fit.as_ref().is_some_and(|fit| fit.enabled))
                    .then_some(id)
            })
            .collect();
        if active.is_empty() {
            return;
        }

        let Some(backend) = self.backend.clone() else {
            return;
        };
        let (Ok(windows), Ok(monitors), Ok(border)) = (
            backend.list_windows(),
            backend.list_monitors(),
            backend.window_border(),
        ) else {
            return;
        };
        let mut publish = false;
        for id in active {
            let panel = &mut self.panels.get_mut(&id).expect("active fit panel");
            let PanelOwnership::Window {
                address,
                panel_workspace_id,
                ..
            } = &panel.ownership
            else {
                continue;
            };
            let fit = panel.fit.as_mut().expect("active fit");
            let window = windows.iter().find(|w| &w.address == address);
            let monitor = monitors.iter().find(|m| m.name == panel.output_name);
            let proposal = window.zip(monitor).and_then(|(window, monitor)| {
                let count = windows
                    .iter()
                    .filter(|w| w.workspace_id == *panel_workspace_id)
                    .count();
                (count == 1
                    && window.workspace_id == *panel_workspace_id
                    && window.monitor_name == monitor.name
                    && window.floating
                    && !window.fullscreen
                    && !window.pinned
                    && monitor.scale == 1.0)
                    .then(|| fitted_size(window, monitor, border))
                    .map(|size| (size, window))
            });
            let (size, window) = match proposal {
                Some((size, window)) if valid_size(size) => (size, window),
                other => {
                    let reason = if other.is_some() {
                        SIZE_REQUIREMENTS
                    } else {
                        "Fit requires one floating, unpinned window on a scale-1 output."
                    };
                    if fit.status.as_deref() != Some(reason) {
                        fit.status = Some(reason.into());
                        publish = true;
                    }
                    fit.observed = None;
                    fit.attempted = None;
                    continue;
                }
            };
            if let Some(deadline) = fit.settle_until {
                if now < deadline {
                    continue;
                }
                fit.settle_until = None;
                if fit.attempted != Some(size) {
                    fit.enabled = false;
                    fit.status = Some(
                        "The app changed size after fitting. Switched to fixed resolution.".into(),
                    );
                    publish = true;
                }
                continue;
            }
            if fit.observed.is_none_or(|(old, _)| old != size) {
                fit.observed = Some((size, now));
                fit.attempted = None;
                continue;
            }
            if fit.attempted == Some(size)
                || now.duration_since(fit.observed.expect("observed window size").1) < FIT_DEBOUNCE
            {
                continue;
            }
            fit.attempted = Some(size);
            let window_before = window.clone();
            let result = self.resize_panel(id, size, Some((&window_before, border)));
            if let Some(fit) = self.panels.get_mut(&id).and_then(|p| p.fit.as_mut()) {
                fit.status = result.as_ref().err().map(|err| format!("{err:#}"));
                if result.is_ok() {
                    fit.settle_until = Some(Instant::now() + FIT_DEBOUNCE);
                }
            }
            publish = true;
            // Re-read topology next time rather than using stale geometry for another candidate.
            break;
        }
        if publish {
            self.broadcast_state();
        }
    }

    fn resize_panel(
        &mut self,
        surface: u32,
        size: [u32; 2],
        fit: Option<(&WindowInfo, u32)>,
    ) -> Result<()> {
        ensure!(valid_size(size), "{SIZE_REQUIREMENTS}");
        ensure!(
            self.visible(surface),
            "Show the virtual panel before changing its resolution."
        );
        let backend = self
            .backend
            .clone()
            .context("Virtual panels are unavailable.")?;
        let panel = self
            .panels
            .get(&surface)
            .context("The virtual panel has closed.")?;
        let name = panel.output_name.clone();
        let monitors = backend.list_monitors()?;
        let index = monitors
            .iter()
            .position(|m| m.name == name)
            .context("Virtual output has disappeared.")?;
        let old = &monitors[index];
        let previous_size = [old.width, old.height];
        if size == previous_size {
            if let Some((window, border)) = fit {
                recenter(backend.as_ref(), old, window, border)?;
            }
            return Ok(());
        }

        let positions = resize_layout(&monitors, index, size).context(
            "That resolution would overlap another output or break mouse access. Move the panel first."
        )?;
        let position = positions[index];
        let updates = |[w, h]: [u32; 2], restore: bool| {
            monitors
                .iter()
                .zip(&positions)
                .enumerate()
                .filter_map(|(i, (monitor, &new_pos))| {
                    (i == index || monitor.logical_pos != new_pos).then(|| OutputConfigUpdate {
                        name: monitor.name.clone(),
                        mode: (i == index).then_some((w, h, 60)),
                        pos: Some(if restore {
                            monitor.logical_pos
                        } else {
                            new_pos
                        }),
                        scale: (i == index).then_some(old.scale),
                    })
                })
                .collect::<Vec<_>>()
        };
        lock(&self.shared.resizing_virtual_outputs).insert(surface);
        lock(&self.shared.virtual_keyframe_sizes).remove(&surface);
        let serial = self
            .shared
            .virtual_resize_ack
            .load(Ordering::Acquire)
            .then(|| {
                let serial = self.next_resize_serial;
                self.next_resize_serial += 1;
                serial
            });
        let pointer = backend.cursor_position().ok().and_then(|pointer| {
            let i = monitors.iter().position(|m| m.contains(pointer))?;
            (i == index || positions[i] != monitors[i].logical_pos).then_some((i, pointer))
        });
        let mut applied = false;
        let result = (|| -> Result<()> {
            backend.configure_outputs(&updates(size, false))?;
            applied = true;
            let confirmed = backend.list_monitors()?;
            let monitor = confirmed
                .iter()
                .find(|m| m.name == name)
                .context("Resized output disappeared.")?;
            ensure!(
                [monitor.width, monitor.height] == size
                    && monitor.logical_pos == position
                    && monitors.iter().zip(&positions).all(|(prev, &pos)| {
                        prev.logical_pos == pos
                            || confirmed
                                .iter()
                                .any(|m| m.name == prev.name && m.logical_pos == pos)
                    }),
                "The compositor did not apply the requested output geometry."
            );
            self.shared
                .update_outputs(&self.output, to_output_geos(&confirmed));
            let recentered = fit
                .map(|(window, border)| {
                    recenter(backend.as_ref(), monitor, window, border).map(|pos| (window, pos))
                })
                .transpose()?;
            if let Some((i, pointer)) = pointer
                && let Some(next) = confirmed.iter().find(|m| m.name == monitors[i].name)
            {
                let (x, y) = monitors[i].logical_pos;
                let (new_x, new_y) = next.logical_pos;
                let (from, to) = match recentered {
                    Some((window, pos))
                        if i == index
                            && (0..2).all(|axis| {
                                pointer[axis] >= window.pos[axis]
                                    && pointer[axis] < window.pos[axis] + window.size[axis] as i32
                            }) =>
                    {
                        (window.pos, pos)
                    }
                    _ => ([x, y], [new_x, new_y]),
                };
                preserve_cursor(backend.as_ref(), next, pointer, from, to);
            }
            if let Some(serial) = serial {
                self.output
                    .send(&wayvr_ipc::frame::HostMessage::VerifyVirtualResize {
                        surface,
                        size,
                        serial,
                    });
            }
            lock(&self.shared.keyframes).insert(surface);
            self.wait_for_frame(surface, size, serial)
        })();

        if let Err(err) = result {
            if !applied {
                lock(&self.shared.resizing_virtual_outputs).remove(&surface);
                return Err(err.context("Kept the previous resolution"));
            }
            let restore = (|| -> Result<()> {
                backend.configure_outputs(&updates(previous_size, true))?;
                if let Some((window, _)) = fit {
                    backend.move_floating_window(&window.address, window.pos)?;
                }
                let confirmed = backend.list_monitors()?;
                self.shared
                    .update_outputs(&self.output, to_output_geos(&confirmed));
                if self.panels.get(&surface).is_some_and(capture_finished) {
                    let cancel = self.panels[&surface].cancel.clone();
                    let capture = self.start_capture(surface, &name, cancel)?;
                    self.panels
                        .get_mut(&surface)
                        .expect("sizing panel")
                        .capture_thread = Some(capture);
                }
                lock(&self.shared.keyframes).insert(surface);
                Ok(())
            })();
            lock(&self.shared.resizing_virtual_outputs).remove(&surface);
            return match restore {
                Ok(()) => Err(err.context("Restored the previous resolution")),
                Err(restore) => Err(err.context(format!(
                    "Could not restore the previous resolution: {restore:#}"
                ))),
            };
        }
        lock(&self.shared.resizing_virtual_outputs).remove(&surface);
        Ok(())
    }

    fn wait_for_frame(
        &self,
        surface: u32,
        [width, height]: [u32; 2],
        serial: Option<u64>,
    ) -> Result<()> {
        let deadline = Instant::now() + STREAM_TIMEOUT;
        while Instant::now() < deadline && !self.output.stopped() {
            if lock(&self.shared.virtual_keyframe_sizes).get(&surface) == Some(&(width, height))
                && serial.is_none_or(|serial| {
                    *lock(&self.shared.decoded_virtual_resize) == Some((surface, serial))
                })
            {
                return Ok(());
            }
            ensure!(
                !self.panels.get(&surface).is_some_and(capture_finished),
                "The encoder stopped."
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::bail!(if serial.is_some() {
            "The headset did not confirm a decoded frame at the new resolution."
        } else {
            "The resized stream did not produce a keyframe."
        })
    }
}

fn capture_finished(panel: &ActivePanel) -> bool {
    panel
        .capture_thread
        .as_ref()
        .is_none_or(|thread| thread.is_finished())
}

fn fitted_size(window: &WindowInfo, monitor: &MonitorGeometry, border: u32) -> [u32; 2] {
    let reserved = monitor.reserved.map(|v| v.max(0) as u32);
    std::array::from_fn(|axis| {
        let outer = window.size[axis]
            .saturating_add(border.saturating_mul(2))
            .saturating_add(reserved[axis])
            .saturating_add(reserved[axis + 2]);
        outer.saturating_add(1) & !1
    })
}

fn recenter(
    backend: &dyn crate::compositor::CompositorBackend,
    monitor: &MonitorGeometry,
    window: &WindowInfo,
    border: u32,
) -> Result<[i32; 2]> {
    let [left, top, right, bottom] = monitor.reserved;
    let (x, y) = monitor.logical_pos;
    let (w, h) = monitor.logical_size;
    let pos = [
        x + left + ((w - left - right - window.size[0] as i32) / 2).max(border as i32),
        y + top + ((h - top - bottom - window.size[1] as i32) / 2).max(border as i32),
    ];
    backend.move_floating_window(&window.address, pos)?;
    Ok(pos)
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn right(self) -> i32 {
        self.x + self.w
    }
    fn bottom(self) -> i32 {
        self.y + self.h
    }

    fn overlaps(self, other: Self) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    fn touches(self, other: Self) -> bool {
        ((self.right() == other.x || other.right() == self.x)
            && self.y.max(other.y) < self.bottom().min(other.bottom()))
            || ((self.bottom() == other.y || other.bottom() == self.y)
                && self.x.max(other.x) < self.right().min(other.right()))
    }
}

fn reachable(rects: &[Rect], physical: &[bool]) -> Vec<bool> {
    let mut reached = physical.to_vec();
    let mut todo: Vec<_> = (0..rects.len()).filter(|&i| physical[i]).collect();
    while let Some(i) = todo.pop() {
        for j in 0..rects.len() {
            if !reached[j] && rects[i].touches(rects[j]) {
                reached[j] = true;
                todo.push(j);
            }
        }
    }
    reached
}

fn resize_layout(
    monitors: &[MonitorGeometry],
    target: usize,
    [w, h]: [u32; 2],
) -> Option<Vec<(i32, i32)>> {
    let physical: Vec<_> = monitors
        .iter()
        .map(|m| !is_virtual_output_name(&m.name))
        .collect();
    let rects: Vec<_> = monitors
        .iter()
        .map(|m| Rect {
            x: m.logical_pos.0,
            y: m.logical_pos.1,
            w: m.logical_size.0,
            h: m.logical_size.1,
        })
        .collect();
    let old = rects[target];
    let connected = reachable(&rects, &physical);
    let scale = monitors[target].scale;
    let w = (w as f32 / scale).round() as i32;
    let h = (h as f32 / scale).round() as i32;

    // Choose an origin along an existing seam. If a straight chain of virtual
    // outputs extends from a moved edge, carry that edge through the chain.
    let mut candidates = vec![(old.x, old.y)];
    let neighbors: Vec<_> = rects
        .iter()
        .enumerate()
        .filter(|&(i, r)| i != target && r.touches(old))
        .collect();
    for &(_, r) in &neighbors {
        if old.right() == r.x {
            candidates.push((r.x - w, old.y.clamp(r.y - h + 1, r.bottom() - 1)));
        }
        if old.x == r.right() {
            candidates.push((r.right(), old.y.clamp(r.y - h + 1, r.bottom() - 1)));
        }
        if old.bottom() == r.y {
            candidates.push((old.x.clamp(r.x - w + 1, r.right() - 1), r.y - h));
        }
        if old.y == r.bottom() {
            candidates.push((old.x.clamp(r.x - w + 1, r.right() - 1), r.bottom()));
        }
    }
    let moves = |placement: &[Rect]| {
        placement
            .iter()
            .zip(&rects)
            .filter(|(next, old)| (next.x, next.y) != (old.x, old.y))
            .count()
    };
    candidates
        .into_iter()
        .flat_map(|(x, y)| {
            let mut placement = rects.clone();
            placement[target] = Rect { x, y, w, h };
            let shifted = shift_neighbors(&rects, placement.clone(), &physical, target);
            [placement, shifted]
        })
        .filter(|placement| {
            let reach = reachable(placement, &physical);
            reach[target]
                && !connected.iter().zip(reach).any(|(&was, now)| was && !now)
                && placement.iter().enumerate().all(|(i, &r)| {
                    placement
                        .iter()
                        .enumerate()
                        .skip(i + 1)
                        .all(|(j, &other)| physical[i] && physical[j] || !r.overlaps(other))
                })
        })
        .min_by_key(|placement| {
            let next = placement[target];
            let lost_edges = neighbors
                .iter()
                .filter(|(i, _)| !next.touches(placement[*i]))
                .count();
            (
                lost_edges,
                moves(placement),
                old.x.abs_diff(next.x) + old.y.abs_diff(next.y),
            )
        })
        .map(|placement| placement.into_iter().map(|r| (r.x, r.y)).collect())
}

fn shift_neighbors(
    old: &[Rect],
    mut next: Vec<Rect>,
    physical: &[bool],
    target: usize,
) -> Vec<Rect> {
    let origin = old[target];
    let resized = next[target];
    for (delta, horizontal, forward) in [
        (resized.x - origin.x, true, false),
        (resized.right() - origin.right(), true, true),
        (resized.y - origin.y, false, false),
        (resized.bottom() - origin.bottom(), false, true),
    ] {
        if delta == 0 {
            continue;
        }
        let mut pending = vec![target];
        let mut visited = physical.to_vec();
        visited[target] = true;
        while let Some(i) = pending.pop() {
            for (j, r) in old.iter().copied().enumerate() {
                let from = old[i];
                let along_edge = match (horizontal, forward) {
                    (true, true) => from.right() == r.x,
                    (true, false) => from.x == r.right(),
                    (false, true) => from.bottom() == r.y,
                    (false, false) => from.y == r.bottom(),
                };
                if visited[j] || !along_edge || !from.touches(r) {
                    continue;
                }
                if horizontal {
                    next[j].x += delta;
                } else {
                    next[j].y += delta;
                }
                visited[j] = true;
                pending.push(j);
            }
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, pos: (i32, i32), size: (i32, i32), scale: f32) -> MonitorGeometry {
        MonitorGeometry {
            id: 0,
            name: name.into(),
            width: (size.0 as f32 * scale) as u32,
            height: (size.1 as f32 * scale) as u32,
            logical_pos: pos,
            logical_size: size,
            scale,
            reserved: [0; 4],
            focused: false,
            active_workspace_id: 0,
            active_workspace_name: String::new(),
        }
    }

    #[test]
    fn resize_preserves_shared_edge_and_shifts_virtual_neighbors() {
        let monitors = [
            monitor("DP-1", (-1440, 0), (1440, 900), 1.5),
            monitor("FRAME-V1", (0, 0), (1280, 720), 1.0),
            monitor("FRAME-V2", (1280, 0), (960, 720), 1.0),
            monitor("FRAME-V3", (2240, 0), (960, 720), 1.0),
        ];
        for width in [960, 1600] {
            assert_eq!(
                resize_layout(&monitors, 1, [width, 960]),
                Some(vec![
                    (-1440, 0),
                    (0, 0),
                    (width as i32, 0),
                    (width as i32 + 960, 0)
                ]),
            );
        }
    }

    #[test]
    fn resize_rejects_crossing_a_physical_anchor() {
        let monitors = [
            monitor("DP-1", (-1440, 0), (1440, 960), 1.0),
            monitor("FRAME-V1", (0, 0), (1280, 720), 1.0),
            monitor("HDMI-A-1", (1280, 0), (1440, 960), 1.0),
        ];
        assert!(resize_layout(&monitors, 1, [1600, 900]).is_none());
    }
}
