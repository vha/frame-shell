//! Virtual headless panel lifecycle manager for interactive VR desktop panels.

mod detach;
mod sizing;
mod window_follow;

use crate::{
    Output, Shared, capture,
    compositor::{
        self, CompositorBackend, CompositorEvent, MonitorGeometry, normalize_window_address,
        wlr_output::OutputConfigUpdate,
    },
    wayland_capture,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
};
use wayvr_ipc::frame::{
    HostMessage, PanelLayoutTarget, PhysicalMonitorInfo, VirtualCapabilities, VirtualPanelInfo,
    VirtualState, VirtualTargetKind, VirtualTargets, VirtualWindowTarget, VirtualWorkspaceTarget,
    virtual_surface_id,
};

#[derive(Debug)]
pub enum Command {
    RequestState,
    RefreshTargets,
    Detach(wayvr_ipc::virtual_panel::DetachGesture),
    Create {
        source: VirtualTargetKind,
        size: [u32; 2],
    },
    Close {
        surface: u32,
    },
    Focus {
        surface: u32,
    },
    SetSizing {
        surface: u32,
        sizing: wayvr_ipc::virtual_panel::Sizing,
    },
    SyncLayout {
        placements: Vec<PanelLayoutTarget>,
        logical_sizes: Vec<(u32, [i32; 2])>,
    },
    Event(CompositorEvent),
    Shutdown,
}

#[derive(Clone)]
enum PanelOwnership {
    Blank {
        workspace_id: i32,
    },
    Window {
        address: String,
        title: String,
        app_id: String,
        panel_workspace_id: i32,
        return_workspace_id: i32,
        return_monitor: String,
        restore_tiled: bool,
    },
    Workspace {
        workspace_id: i32,
        workspace_name: String,
        return_monitor: String,
    },
}

struct ActivePanel {
    surface: u32,
    output_name: String,
    title: String,
    width: u32,
    height: u32,
    logical_pos: [i32; 2],
    logical_size: [i32; 2],
    scale: f32,
    ownership: PanelOwnership,
    cancel: Arc<AtomicBool>,
    capture_thread: Option<JoinHandle<()>>,
    fit: Option<sizing::FitWindow>,
}

impl ActivePanel {
    fn info(&self) -> VirtualPanelInfo {
        VirtualPanelInfo {
            surface: self.surface,
            output_name: self.output_name.clone(),
            title: self.title.clone(),
            width: self.width,
            height: self.height,
            logical_pos: self.logical_pos,
            logical_size: self.logical_size,
            scale: self.scale,
            fit_to_window: self.fit.as_ref().map(|fit| fit.enabled),
            fit_status: self.fit.as_ref().and_then(|fit| fit.status.clone()),
        }
    }
}

pub struct VirtualManager {
    tx: Sender<Command>,
    #[cfg(test)]
    capabilities: VirtualCapabilities,
}

impl VirtualManager {
    /// Initializes virtual panel support, synchronously cleans up any leftover `FRAME-V*` outputs
    /// before physical capture starts, and spawns the virtual-panel manager worker thread.
    pub fn start(output: Output, shared: Arc<Shared>) -> (Self, JoinHandle<()>) {
        let (backend, capabilities) = compositor::detect();
        if let Some(ref backend) = backend
            && let Err(err) = orphan_sweep(backend.as_ref())
        {
            eprintln!("Virtual panel orphan sweep failed: {err:#}");
        }
        let (tx, rx) = mpsc::channel();
        let tx_events = tx.clone();
        #[cfg(test)]
        let test_caps = capabilities.clone();
        let handle = std::thread::Builder::new()
            .name("frame-virtual".into())
            .spawn(move || {
                run_worker(backend, capabilities, output, shared, rx, tx_events);
            })
            .expect("spawning frame-virtual thread");
        (
            Self {
                tx,
                #[cfg(test)]
                capabilities: test_caps,
            },
            handle,
        )
    }

    pub fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    #[cfg(test)]
    pub fn capabilities(&self) -> &VirtualCapabilities {
        &self.capabilities
    }
}

/// Evacuates any windows on leftover `FRAME-V*` outputs and removes those headless outputs.
pub fn orphan_sweep(backend: &dyn CompositorBackend) -> Result<()> {
    let monitors = backend.list_monitors()?;
    let orphans: Vec<MonitorGeometry> = monitors
        .iter()
        .filter(|m| capture::is_virtual_output_name(&m.name))
        .cloned()
        .collect();
    if orphans.is_empty() {
        return Ok(());
    }
    let fallback_ws = pick_primary_physical(&monitors)
        .map(|m| m.active_workspace_id.max(1))
        .unwrap_or(1);
    if let Ok(windows) = backend.list_windows() {
        for win in windows {
            if capture::is_virtual_output_name(&win.monitor_name) {
                let _ = backend.move_window_to_workspace(&win.address, &fallback_ws.to_string());
            }
        }
    }
    for orphan in orphans {
        eprintln!("Cleaning up orphaned virtual output {}", orphan.name);
        let _ = backend.remove_output(&orphan.name);
    }
    Ok(())
}

fn pick_primary_physical(monitors: &[MonitorGeometry]) -> Option<&MonitorGeometry> {
    monitors
        .iter()
        .filter(|m| !capture::is_virtual_output_name(&m.name))
        .min_by_key(|m| (m.logical_pos != (0, 0), !m.focused, m.id))
}

fn to_output_geos(monitors: &[MonitorGeometry]) -> Vec<capture::OutputGeometry> {
    monitors
        .iter()
        .map(|m| capture::OutputGeometry {
            name: m.name.clone(),
            logical_pos: m.logical_pos,
            logical_size: m.logical_size,
            physical_size: (m.width as i32, m.height as i32),
        })
        .collect()
}

struct Worker {
    backend: Option<Arc<dyn CompositorBackend>>,
    capabilities: VirtualCapabilities,
    output: Output,
    shared: Arc<Shared>,
    panels: BTreeMap<u32, ActivePanel>,
    next_slot: u32,
    next_fit_poll: std::time::Instant,
    detach: Option<detach::Gesture>,
    last_detach_release: u64,
    next_detach_poll: std::time::Instant,
    last_fit_layout: Vec<MonitorGeometry>,
    next_resize_serial: u64,
}

impl Worker {
    fn physical_monitors_info(&self, monitors: &[MonitorGeometry]) -> Vec<PhysicalMonitorInfo> {
        let layout = crate::lock(&self.shared.output_layout);
        monitors
            .iter()
            .filter(|m| !capture::is_virtual_output_name(&m.name))
            .map(|m| PhysicalMonitorInfo {
                surface: layout.stream_id_for_output(&m.name),
                name: m.name.clone(),
                width: m.width,
                height: m.height,
                logical_pos: [m.logical_pos.0, m.logical_pos.1],
                logical_size: [m.logical_size.0, m.logical_size.1],
                scale: m.scale,
            })
            .collect()
    }

    fn refresh_panel_geometries(&mut self, monitors: &[MonitorGeometry]) {
        for panel in self.panels.values_mut() {
            if let Some(m) = monitors.iter().find(|m| m.name == panel.output_name) {
                panel.width = m.width;
                panel.height = m.height;
                panel.logical_pos = [m.logical_pos.0, m.logical_pos.1];
                panel.logical_size = [m.logical_size.0, m.logical_size.1];
                panel.scale = m.scale;
            }
        }
    }

    fn broadcast_state(&mut self) {
        let monitors = self
            .backend
            .as_ref()
            .and_then(|b| b.list_monitors().ok())
            .unwrap_or_default();
        if !monitors.is_empty() {
            self.shared
                .update_outputs(&self.output, to_output_geos(&monitors));
            self.refresh_panel_geometries(&monitors);
        }
        let physical_monitors = self.physical_monitors_info(&monitors);
        let panels = self.panels.values().map(ActivePanel::info).collect();
        self.output.send(&HostMessage::VirtualState(VirtualState {
            capabilities: self.capabilities.clone(),
            panels,
            physical_monitors,
        }));
    }

    fn broadcast_targets(&self) {
        let Some(ref backend) = self.backend else {
            return;
        };
        let windows = match backend.list_windows() {
            Ok(w) => w,
            Err(err) => {
                eprintln!("Virtual list_windows failed: {err:#}");
                Vec::new()
            }
        };
        let workspaces = match backend.list_workspaces() {
            Ok(ws) => ws,
            Err(err) => {
                eprintln!("Virtual list_workspaces failed: {err:#}");
                Vec::new()
            }
        };
        let owned_workspaces: Vec<i32> = self
            .panels
            .values()
            .filter_map(|p| match &p.ownership {
                PanelOwnership::Blank { workspace_id } => Some(*workspace_id),
                _ => None,
            })
            .collect();

        let windows = windows
            .into_iter()
            .map(|w| VirtualWindowTarget {
                already_shared: self.window_panel(&w.address).is_some(),
                address: w.address,
                title: w.title,
                app_id: w.app_id,
                workspace_id: w.workspace_id,
                workspace_name: w.workspace_name,
                monitor: w.monitor_name,
            })
            .collect();
        let workspaces = workspaces
            .into_iter()
            .filter(|ws| !owned_workspaces.contains(&ws.id))
            .map(|ws| VirtualWorkspaceTarget {
                already_shared: self.workspace_panel(ws.id).is_some(),
                id: ws.id,
                name: ws.name,
                monitor: ws.monitor_name,
                windows: ws.windows,
            })
            .collect();
        self.output
            .send(&HostMessage::VirtualTargets(VirtualTargets {
                windows,
                workspaces,
            }));
    }

    fn window_panel(&self, address: &str) -> Option<&ActivePanel> {
        let address = normalize_window_address(address);
        self.panels.values().find(|panel| {
            matches!(&panel.ownership, PanelOwnership::Window { address: owned, .. } if *owned == address)
        })
    }

    fn workspace_panel(&self, id: i32) -> Option<&ActivePanel> {
        self.panels.values().find(|panel| {
            matches!(panel.ownership, PanelOwnership::Workspace { workspace_id, .. } if workspace_id == id)
        })
    }

    fn focus_panel(&self, surface: u32) {
        let (Some(backend), Some(panel)) = (&self.backend, self.panels.get(&surface)) else {
            return;
        };
        self.output.send(&HostMessage::RevealVirtualPanel(surface));
        if let Err(err) = backend.focus_output(&panel.output_name) {
            self.output.send(&HostMessage::Error(format!("Could not focus virtual panel: {err:#}")));
        }
    }

    fn handle_create(&mut self, source: VirtualTargetKind, size: [u32; 2]) {
        self.create_panel(source, size, None);
    }

    fn create_panel(
        &mut self,
        source: VirtualTargetKind,
        size: [u32; 2],
        detach: Option<detach::Creation>,
    ) {
        let Some(backend) = self.backend.clone() else {
            self.output.send(&HostMessage::Error(
                self.capabilities
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Virtual panels require Hyprland.".into()),
            ));
            return;
        };
        if !self.capabilities.supported {
            self.output.send(&HostMessage::Error(
                self.capabilities
                    .reason
                    .clone()
                    .unwrap_or_else(|| "Virtual panels are unavailable on this host.".into()),
            ));
            return;
        }
        let existing = match &source {
            VirtualTargetKind::Blank { .. } => None,
            VirtualTargetKind::Window { address, .. } => self.window_panel(address),
            VirtualTargetKind::Workspace { id, .. } => self.workspace_panel(*id),
        };
        if let Some(panel) = existing {
            self.focus_panel(panel.surface);
            return;
        }

        if self.panels.len() >= self.capabilities.max_panels as usize {
            self.output.send(&HostMessage::Error(format!(
                "Virtual panel limit reached ({}/{})",
                self.panels.len(),
                self.capabilities.max_panels
            )));
            self.broadcast_state();
            return;
        }

        let Some(surface) = virtual_surface_id(self.next_slot) else {
            self.output.send(&HostMessage::Error(
                "Virtual surface ID limit reached".into(),
            ));
            return;
        };
        self.next_slot += 1;
        let output_name = format!("FRAME-V{}", self.next_slot);
        let req_size = match &source {
            VirtualTargetKind::Blank { width, height } if *width > 0 && *height > 0 => {
                [*width, *height]
            }
            _ => size,
        };
        let [width, height] = wayvr_ipc::virtual_panel::normalized_size(req_size);

        let attempt = (|| -> Result<ActivePanel> {
            let monitors = backend.list_monitors()?;
            let primary_phys =
                pick_primary_physical(&monitors).context("No physical monitor available")?;
            let primary_phys_name = primary_phys.name.clone();
            let primary_phys_ws = primary_phys.active_workspace_id.max(1);
            let spawn_x = monitors
                .iter()
                .map(|m| m.logical_pos.0 + m.logical_size.0)
                .max()
                .unwrap_or(0);
            let (spawn_x, spawn_y) = detach
                .as_ref()
                .map_or((spawn_x, primary_phys.logical_pos.1), |drop| drop.position);

            let (title, ownership) = match &source {
                VirtualTargetKind::Blank { .. } => (
                    output_name.clone(),
                    PanelOwnership::Blank { workspace_id: -1 },
                ),
                VirtualTargetKind::Window { address, .. } => {
                    let norm = normalize_window_address(address);
                    let win = backend
                        .list_windows()?
                        .into_iter()
                        .find(|w| w.address == norm)
                        .context("Selected window is no longer open")?;
                    let return_monitor = if let Some(drop) = &detach {
                        drop.original_monitor.clone()
                    } else if capture::is_virtual_output_name(&win.monitor_name)
                        || win.monitor_name.is_empty()
                    {
                        primary_phys_name.clone()
                    } else {
                        win.monitor_name.clone()
                    };
                    let return_ws = if let Some(drop) = &detach {
                        drop.original_workspace
                    } else if capture::is_virtual_output_name(&win.monitor_name) {
                        primary_phys_ws
                    } else {
                        win.workspace_id.max(1)
                    };
                    let label = if !win.title.is_empty() {
                        win.title.clone()
                    } else if !win.app_id.is_empty() {
                        win.app_id.clone()
                    } else {
                        output_name.clone()
                    };
                    (
                        label,
                        PanelOwnership::Window {
                            address: norm,
                            title: win.title,
                            app_id: win.app_id,
                            panel_workspace_id: -1,
                            return_workspace_id: return_ws,
                            return_monitor,
                            restore_tiled: detach.as_ref().is_some_and(|drop| drop.restore_tiled),
                        },
                    )
                }
                VirtualTargetKind::Workspace { id, name } => {
                    let ws = backend
                        .list_workspaces()?
                        .into_iter()
                        .find(|w| w.id == *id)
                        .with_context(|| format!("Workspace {id} no longer exists"))?;
                    let return_monitor = if capture::is_virtual_output_name(&ws.monitor_name)
                        || ws.monitor_name.is_empty()
                    {
                        primary_phys_name.clone()
                    } else {
                        ws.monitor_name.clone()
                    };
                    let label = if !ws.name.is_empty() {
                        format!("Workspace {}", ws.name)
                    } else if !name.is_empty() {
                        format!("Workspace {name}")
                    } else {
                        format!("Workspace {id}")
                    };
                    (
                        label,
                        PanelOwnership::Workspace {
                            workspace_id: ws.id,
                            workspace_name: ws.name,
                            return_monitor,
                        },
                    )
                }
            };

            // Creating a headless output can reset existing custom modes before
            // the new layout is applied, so pin the pre-creation geometry.
            let pin_updates: Vec<OutputConfigUpdate> = monitors
                .iter()
                .map(|m| OutputConfigUpdate {
                    name: m.name.clone(),
                    mode: capture::is_virtual_output_name(&m.name)
                        .then_some((m.width, m.height, 60)),
                    pos: Some(m.logical_pos),
                    scale: Some(m.scale),
                })
                .collect();

            backend.create_headless_output(&output_name)?;
            let setup_res = (|| -> Result<PanelOwnership> {
                let mut config_updates = pin_updates.clone();
                config_updates.push(OutputConfigUpdate {
                    name: output_name.clone(),
                    mode: Some((width, height, 60)),
                    pos: Some((spawn_x, spawn_y)),
                    scale: Some(1.0),
                });
                backend.configure_outputs(&config_updates)?;

                let monitors_after = if detach.is_some() {
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(500);
                    loop {
                        let current = backend.list_monitors()?;
                        if current.iter().any(|m| {
                            m.name == output_name
                                && [m.width, m.height] == [width, height]
                                && m.logical_pos == (spawn_x, spawn_y)
                                && m.active_workspace_id > 0
                        }) && sizing::layout_has_no_virtual_overlaps(&current, &[])
                        {
                            break current;
                        }
                        ensure!(
                            std::time::Instant::now() < deadline,
                            "The compositor did not confirm a usable drop position and workspace"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                } else {
                    backend.list_monitors()?
                };
                let vmon = monitors_after
                    .iter()
                    .find(|m| m.name == output_name)
                    .with_context(|| format!("Created output {output_name} missing"))?;
                let default_ws_id = vmon.active_workspace_id.max(1);

                match ownership {
                    PanelOwnership::Blank { .. } => Ok(PanelOwnership::Blank {
                        workspace_id: default_ws_id,
                    }),
                    PanelOwnership::Window {
                        address,
                        title,
                        app_id,
                        return_workspace_id,
                        return_monitor,
                        restore_tiled,
                        ..
                    } => {
                        backend.move_window_to_workspace(&address, &default_ws_id.to_string())?;
                        if let Some(drop) = &detach {
                            if restore_tiled {
                                backend.set_window_floating(&address, true)?;
                                backend.resize_floating_window(&address, drop.window.size)?;
                            }
                            let pos = sizing::recenter(
                                backend.as_ref(),
                                vmon,
                                &drop.window,
                                drop.border,
                            )?;
                            preserve_cursor(
                                backend.as_ref(),
                                vmon,
                                drop.cursor,
                                drop.window.pos,
                                pos,
                            );
                            self.output.send(&HostMessage::DetachedPanel {
                                surface,
                                placement: drop.placement.clone(),
                            });
                        }
                        Ok(PanelOwnership::Window {
                            address,
                            title,
                            app_id,
                            panel_workspace_id: default_ws_id,
                            return_workspace_id,
                            return_monitor,
                            restore_tiled,
                        })
                    }
                    PanelOwnership::Workspace {
                        workspace_id,
                        workspace_name,
                        return_monitor,
                    } => {
                        backend.move_workspace_to_output(workspace_id, &output_name)?;
                        Ok(PanelOwnership::Workspace {
                            workspace_id,
                            workspace_name,
                            return_monitor,
                        })
                    }
                }
            })();

            let ownership = match setup_res {
                Ok(own) => own,
                Err(err) => {
                    if let Some(drop) = &detach {
                        drop.restore_window(backend.as_ref());
                    }
                    let _ = backend.remove_output(&output_name);
                    if detach.is_some() {
                        let _ = backend.configure_outputs(&pin_updates);
                    }
                    return Err(err);
                }
            };

            let updated_monitors = backend.list_monitors()?;
            self.shared
                .update_outputs(&self.output, to_output_geos(&updated_monitors));
            let vmon = updated_monitors
                .iter()
                .find(|m| m.name == output_name)
                .cloned();
            let (actual_w, actual_h, pos, logical_size, scale) = vmon.map_or(
                (
                    width,
                    height,
                    [spawn_x, spawn_y],
                    [width as i32, height as i32],
                    1.0,
                ),
                |m| {
                    (
                        m.width,
                        m.height,
                        [m.logical_pos.0, m.logical_pos.1],
                        [m.logical_size.0, m.logical_size.1],
                        m.scale,
                    )
                },
            );

            self.shared.register_stream(surface, output_name.clone());
            let cancel = Arc::new(AtomicBool::new(false));
            let capture_thread = match self.start_capture(surface, &output_name, cancel.clone()) {
                Ok(thread) => thread,
                Err(error) => {
                    self.shared.unregister_stream(surface);
                    if let Some(drop) = &detach {
                        drop.restore_window(backend.as_ref());
                        let _ = backend.remove_output(&output_name);
                        let _ = backend.configure_outputs(&pin_updates);
                    }
                    return Err(error);
                }
            };
            let fit =
                matches!(ownership, PanelOwnership::Window { .. }).then(sizing::FitWindow::default);

            Ok(ActivePanel {
                surface,
                output_name,
                title,
                width: actual_w,
                height: actual_h,
                logical_pos: pos,
                logical_size,
                scale,
                ownership,
                cancel,
                capture_thread: Some(capture_thread),
                fit,
            })
        })();

        match attempt {
            Ok(panel) => {
                eprintln!(
                    "Created virtual panel {} (surface {surface:#x}, {}x{} at {:?})",
                    panel.output_name, panel.width, panel.height, panel.logical_pos
                );
                self.panels.insert(surface, panel);
                if let Some(drop) = &detach {
                    self.broadcast_state();
                    let serial =
                        self.shared
                            .virtual_resize_ack
                            .load(Ordering::Acquire)
                            .then(|| {
                                let serial = self.next_resize_serial;
                                self.next_resize_serial += 1;
                                self.output.send(&HostMessage::VerifyVirtualResize {
                                    surface,
                                    size,
                                    serial,
                                });
                                crate::lock(&self.shared.keyframes).insert(surface);
                                serial
                            });
                    if let Err(error) = self.wait_for_frame(surface, size, serial) {
                        self.close_panel(surface, true, true);
                        if !drop.restore_tiled {
                            let _ = backend
                                .move_floating_window(&drop.window.address, drop.original_position);
                        }
                        self.output.send(&HostMessage::Error(format!(
                            "Could not stream the detached window: {error:#}"
                        )));
                        return;
                    }
                }
                self.broadcast_state();
                self.broadcast_targets();
            }
            Err(err) => {
                eprintln!("Failed to create virtual panel: {err:#}");
                self.output.send(&HostMessage::Error(format!(
                    "Virtual panel failed: {err:#}"
                )));
                self.broadcast_state();
                self.broadcast_targets();
            }
        }
    }

    fn start_capture(
        &self,
        surface: u32,
        name: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<JoinHandle<()>> {
        let output = self.output.clone();
        let shared = self.shared.clone();
        let name = name.to_owned();
        Ok(std::thread::Builder::new()
            .name(format!("vcap-{name}"))
            .spawn(move || {
                if let Err(err) = wayland_capture::stream_virtual_output(
                    surface, &name, &cancel, &output, &shared,
                ) {
                    eprintln!("Virtual capture {name} failed: {err:#}");
                    if !crate::lock(&shared.resizing_virtual_outputs).contains(&surface) {
                        output.send(&HostMessage::Error(format!(
                            "Virtual panel {name} capture stopped: {err:#}"
                        )));
                    }
                }
                crate::lock(&shared.virtual_keyframe_sizes).remove(&surface);
                // A sizing transaction restores and restarts this worker on failure.
                if !crate::lock(&shared.resizing_virtual_outputs).contains(&surface) {
                    shared.unregister_stream(surface);
                    output.removed(surface);
                }
            })?)
    }

    fn evacuate_output(
        &self,
        backend: &dyn CompositorBackend,
        panel: &ActivePanel,
        restore_owned: bool,
    ) {
        let Ok(monitors) = backend.list_monitors() else {
            return;
        };
        let Some(fallback) = pick_primary_physical(&monitors).cloned() else {
            return;
        };
        let fallback_ws = fallback.active_workspace_id.max(1);
        let windows = backend.list_windows().unwrap_or_default();

        match &panel.ownership {
            PanelOwnership::Blank { workspace_id } => {
                for win in &windows {
                    if win.monitor_name == panel.output_name || win.workspace_id == *workspace_id {
                        let _ = backend
                            .move_window_to_workspace(&win.address, &fallback_ws.to_string());
                    }
                }
            }
            PanelOwnership::Window {
                address,
                panel_workspace_id,
                return_workspace_id,
                return_monitor,
                restore_tiled,
                ..
            } => {
                let target_ws = if restore_owned {
                    let return_mon_exists = monitors.iter().any(|m| {
                        m.name != panel.output_name && m.name == *return_monitor
                    });
                    if return_mon_exists {
                        *return_workspace_id
                    } else {
                        fallback_ws
                    }
                } else {
                    fallback_ws
                };
                for win in &windows {
                    if win.address == *address && restore_owned {
                        if backend
                            .move_window_to_workspace(&win.address, &target_ws.to_string())
                            .is_ok()
                            && *restore_tiled
                        {
                            let _ = backend.set_window_floating(&win.address, false);
                        }
                    } else if win.monitor_name == panel.output_name || win.workspace_id == *panel_workspace_id
                    {
                        let _ = backend
                            .move_window_to_workspace(&win.address, &fallback_ws.to_string());
                    }
                }
            }
            PanelOwnership::Workspace {
                workspace_id,
                return_monitor,
                ..
            } => {
                let target_monitor = monitors
                    .iter()
                    .find(|m| {
                        !capture::is_virtual_output_name(&m.name) && m.name == *return_monitor
                    })
                    .map_or(fallback.name.as_str(), |m| m.name.as_str());
                let ws_exists = backend
                    .list_workspaces()
                    .unwrap_or_default()
                    .iter()
                    .any(|ws| ws.id == *workspace_id);
                if ws_exists {
                    let _ = backend.move_workspace_to_output(*workspace_id, target_monitor);
                }
                for win in &windows {
                    if win.monitor_name == panel.output_name && win.workspace_id != *workspace_id {
                        let _ = backend
                            .move_window_to_workspace(&win.address, &fallback_ws.to_string());
                    }
                }
            }
        }
    }

    fn close_panel(&mut self, surface: u32, restore_owned: bool, broadcast: bool) {
        let Some(mut panel) = self.panels.remove(&surface) else {
            return;
        };
        panel.cancel.store(true, Ordering::Release);
        if let Some(handle) = panel.capture_thread.take() {
            let _ = handle.join();
        }
        self.shared.unregister_stream(surface);
        crate::lock(&self.shared.visibility).remove(&surface);
        crate::lock(&self.shared.keyframes).remove(&surface);
        crate::lock(&self.shared.virtual_keyframe_sizes).remove(&surface);

        if let Some(backend) = self.backend.as_ref() {
            let before_monitors = backend.list_monitors().unwrap_or_default();
            self.evacuate_output(backend.as_ref(), &panel, restore_owned);
            let _ = backend.remove_output(&panel.output_name);
            let after_monitors = backend.list_monitors().unwrap_or_default();
            let restore_updates: Vec<OutputConfigUpdate> = before_monitors
                .iter()
                .filter(|b| b.name != panel.output_name)
                .filter_map(|b| {
                    let after = after_monitors.iter().find(|a| a.name == b.name)?;
                    let mode_changed = capture::is_virtual_output_name(&b.name)
                        && (after.width, after.height) != (b.width, b.height);
                    (mode_changed || after.logical_pos != b.logical_pos).then(|| OutputConfigUpdate {
                        name: b.name.clone(),
                        mode: mode_changed.then_some((b.width, b.height, 60)),
                        pos: Some(b.logical_pos),
                        scale: Some(b.scale),
                    })
                })
                .collect();
            if !restore_updates.is_empty() {
                let _ = backend.configure_outputs(&restore_updates);
            }
            if let Ok(monitors) = backend.list_monitors() {
                self.shared
                    .update_outputs(&self.output, to_output_geos(&monitors));
            }
        }
        self.output.removed(surface);
        eprintln!("Closed virtual panel {}", panel.output_name);
        if broadcast {
            self.broadcast_state();
            self.broadcast_targets();
        }
    }

    fn handle_sync_layout(
        &mut self,
        placements: Vec<PanelLayoutTarget>,
        logical_sizes: Vec<(u32, [i32; 2])>,
    ) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let Ok(monitors) = backend.list_monitors() else {
            return;
        };
        if logical_sizes.iter().any(|(surface, size)| {
            self.panels.get(surface).is_none_or(|p| {
                !monitors.iter().any(|m| {
                    p.output_name == m.name && [m.logical_size.0, m.logical_size.1] == *size
                })
            })
        }) {
            self.broadcast_state();
            return;
        }
        let mut updates = Vec::new();
        for target in &placements {
            if let Some(panel) = self.panels.get(&target.surface)
                && let Some(monitor) = monitors.iter().find(|m| m.name == panel.output_name)
                && monitor.logical_pos != (target.x, target.y)
            {
                updates.push(OutputConfigUpdate {
                    name: panel.output_name.clone(),
                    mode: None,
                    pos: Some((target.x, target.y)),
                    scale: None,
                });
            }
        }
        if updates.is_empty() {
            self.broadcast_state();
            return;
        }
        if !sizing::layout_has_no_virtual_overlaps(&monitors, &updates) {
            let message = "Skipped virtual panel layout sync because the outputs would overlap.";
            eprintln!("{message}");
            self.output.send(&HostMessage::Error(message.into()));
            self.broadcast_state();
            return;
        }
        let cursor = backend.cursor_position().ok().and_then(|pointer| {
            let old = monitors.iter().find(|monitor| {
                monitor.contains(pointer) && updates.iter().any(|u| u.name == monitor.name)
            })?;
            Some((pointer, old))
        });
        match backend.configure_outputs(&updates) {
            Ok(()) => {
                if let Ok(monitors) = backend.list_monitors() {
                    self.shared
                        .update_outputs(&self.output, to_output_geos(&monitors));
                    if let Some((pointer, old)) = cursor
                        && let Some(new) = monitors.iter().find(|m| m.name == old.name)
                    {
                        preserve_cursor(
                            backend.as_ref(),
                            new,
                            pointer,
                            [old.logical_pos.0, old.logical_pos.1],
                            [new.logical_pos.0, new.logical_pos.1],
                        );
                    }
                    self.refresh_panel_geometries(&monitors);
                }
            }
            Err(err) => {
                eprintln!("Virtual panel layout sync failed: {err:#}");
                self.output.send(&HostMessage::Error(format!(
                    "Failed to sync virtual panel layout: {err:#}"
                )));
            }
        }
        self.broadcast_state();
    }

    fn handle_event(&mut self, event: CompositorEvent) {
        match event {
            CompositorEvent::MonitorLayoutChanged => self.broadcast_state(),
            CompositorEvent::WindowOpened => self.broadcast_targets(),
            CompositorEvent::WindowClosed { address } => {
                let norm = normalize_window_address(&address);
                let victim = self.panels.iter().find_map(|(&id, p)| {
                    matches!(&p.ownership, PanelOwnership::Window { address: a, .. } if *a == norm)
                        .then_some(id)
                });
                if let Some(surface) = victim {
                    eprintln!("Popped-out window {norm} closed; closing virtual panel");
                    self.close_panel(surface, false, true);
                } else {
                    self.broadcast_targets();
                }
            }
            CompositorEvent::WindowMoved {
                address,
                workspace_id,
                ..
            } => {
                let norm = normalize_window_address(&address);
                let moved_off = self.panels.iter().find_map(|(&id, p)| {
                    matches!(
                        &p.ownership,
                        PanelOwnership::Window {
                            address: a,
                            panel_workspace_id,
                            ..
                        } if *a == norm && *panel_workspace_id != workspace_id
                    )
                    .then_some(id)
                });
                if let Some(surface) = moved_off {
                    if self.follow_window_across_outputs(surface) {
                        self.broadcast_targets();
                        return;
                    }
                    eprintln!(
                        "Popped-out window {norm} moved away from virtual workspace; closing virtual panel"
                    );
                    self.close_panel(surface, false, true);
                } else {
                    self.broadcast_targets();
                }
            }
            CompositorEvent::MonitorRemoved { name } => {
                if let Some(surface) = self
                    .panels
                    .iter()
                    .find_map(|(&id, p)| (p.output_name == name).then_some(id))
                {
                    eprintln!("Virtual output {name} removed externally; closing panel");
                    self.close_panel(surface, true, true);
                } else {
                    self.broadcast_state();
                }
            }
        }
    }

    fn shutdown_all(&mut self) {
        let ids: Vec<u32> = self.panels.keys().copied().collect();
        for id in ids {
            self.close_panel(id, true, false);
        }
    }
}

fn preserve_cursor(
    backend: &dyn CompositorBackend,
    monitor: &MonitorGeometry,
    pointer: [i32; 2],
    from: [i32; 2],
    to: [i32; 2],
) {
    let target = std::array::from_fn(|i| pointer[i] - from[i] + to[i]);
    if target != pointer
        && monitor.contains(target)
        && backend.cursor_position().ok() == Some(pointer)
    {
        let _ = backend.move_cursor(target);
    }
}

fn run_worker(
    backend: Option<Arc<dyn CompositorBackend>>,
    capabilities: VirtualCapabilities,
    output: Output,
    shared: Arc<Shared>,
    rx: Receiver<Command>,
    tx_events: Sender<Command>,
) {
    let event_stop = Arc::new(AtomicBool::new(false));
    let event_thread = backend.as_ref().map(|b| {
        let b = b.clone();
        let stop = event_stop.clone();
        let (ev_tx, ev_rx) = mpsc::channel();
        let fwd_tx = tx_events.clone();
        let sub_handle = std::thread::Builder::new()
            .name("frame-hypr-events".into())
            .spawn(move || {
                let _ = b.subscribe_events(ev_tx, stop);
            })
            .expect("spawning event listener");
        let fwd_handle = std::thread::Builder::new()
            .name("frame-hypr-fwd".into())
            .spawn(move || {
                while let Ok(ev) = ev_rx.recv() {
                    if fwd_tx.send(Command::Event(ev)).is_err() {
                        break;
                    }
                }
            })
            .expect("spawning event forwarder");
        (sub_handle, fwd_handle)
    });

    let mut worker = Worker {
        backend,
        capabilities,
        output,
        shared,
        panels: BTreeMap::new(),
        next_slot: 0,
        next_fit_poll: std::time::Instant::now(),
        detach: None,
        last_detach_release: 0,
        next_detach_poll: std::time::Instant::now(),
        last_fit_layout: Vec::new(),
        next_resize_serial: 1,
    };
    worker.broadcast_state();

    let mut next_command = None;
    loop {
        worker.poll_detach();
        worker.poll_window_fit();
        let wait = if worker.detach.is_some() {
            worker.fit_poll_delay().min(detach::POLL_INTERVAL)
        } else { worker.fit_poll_delay() };
        let cmd = match next_command
            .take()
            .map_or_else(|| rx.recv_timeout(wait), Ok)
        {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) if !worker.output.stopped() => continue,
            Err(_) => break,
        };
        if worker.output.stopped() && !matches!(cmd, Command::Shutdown) {
            break;
        }
        match cmd {
            Command::RequestState => {
                worker.broadcast_state();
                worker.broadcast_targets();
            }
            Command::RefreshTargets => worker.broadcast_targets(),
            Command::Detach(gesture) => worker.detach_gesture(gesture),
            Command::Create { source, size } => worker.handle_create(source, size),
            Command::Close { surface } => worker.close_panel(surface, true, true),
            Command::Focus { surface } => worker.focus_panel(surface),
            Command::SetSizing {
                surface,
                mut sizing,
            } => {
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        Command::SetSizing {
                            surface: next,
                            sizing: newer,
                        } if next == surface => sizing = newer,
                        other => {
                            next_command = Some(other);
                            break;
                        }
                    }
                }
                worker.set_sizing(surface, sizing);
            }
            Command::SyncLayout {
                placements,
                logical_sizes,
            } => worker.handle_sync_layout(placements, logical_sizes),
            Command::Event(ev) => worker.handle_event(ev),
            Command::Shutdown => break,
        }
    }

    worker.shutdown_all();
    event_stop.store(true, Ordering::Release);
    drop(tx_events);
    if let Some((sub, fwd)) = event_thread {
        let _ = sub.join();
        let _ = fwd.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    use std::{collections::BTreeSet, ffi::c_void};

    #[ignore = "requires a running Hyprland desktop session and GStreamer hardware encoder"]
    #[test]
    fn live_hyprland_virtual_panel_lifecycle() {
        #[derive(Default)]
        struct Events {
            messages: Vec<HostMessage>,
            surfaces: Vec<u32>,
            removed: Vec<u32>,
            videos: BTreeSet<u32>,
        }

        extern "C" fn on_surface(user: *mut c_void, surface: *const frame_abi::frame_host_surface) {
            let events = unsafe { &*(user as *const Mutex<Events>) };
            let surface = unsafe { &*surface };
            lock(events).surfaces.push(surface.id);
        }
        extern "C" fn on_region(_user: *mut c_void, _id: u32, _region: *const f32) {}
        extern "C" fn on_removed(user: *mut c_void, id: u32) {
            let events = unsafe { &*(user as *const Mutex<Events>) };
            lock(events).removed.push(id);
        }
        extern "C" fn on_video(user: *mut c_void, id: u32, _data: *const u8, size: usize) {
            if size > 0 {
                let events = unsafe { &*(user as *const Mutex<Events>) };
                lock(events).videos.insert(id);
            }
        }
        extern "C" fn on_message(user: *mut c_void, data: *const u8, size: usize) {
            let slice = unsafe { std::slice::from_raw_parts(data, size) };
            if let Ok(msg) = HostMessage::decode(slice) {
                let events = unsafe { &*(user as *const Mutex<Events>) };
                lock(events).messages.push(msg);
            }
        }

        struct CleanupGuard {
            output: Option<Output>,
            manager: Option<VirtualManager>,
            handle: Option<std::thread::JoinHandle<()>>,
            user_ptr: *mut Mutex<Events>,
        }
        impl Drop for CleanupGuard {
            fn drop(&mut self) {
                if let Some(out) = self.output.take() {
                    out.stop();
                }
                if let Some(mgr) = self.manager.take() {
                    mgr.send(Command::Shutdown);
                }
                if let Some(handle) = self.handle.take() {
                    let _ = handle.join();
                }
                unsafe {
                    drop(Box::from_raw(self.user_ptr));
                }
            }
        }

        let (backend, _) = compositor::detect();
        let backend = backend.expect("Hyprland compositor");
        let monitors = backend.list_monitors().unwrap();
        assert!(
            monitors.iter().all(|m| !capture::is_virtual_output_name(&m.name)),
            "Close existing Frame Shell panels before running the live lifecycle test"
        );
        let anchor = monitors.iter().find(|m| m.focused).or_else(|| monitors.first()).unwrap();
        let workspace_id = anchor.active_workspace_id;
        let source = VirtualTargetKind::Workspace {
            id: workspace_id,
            name: workspace_id.to_string(),
        };
        gstreamer::init().expect("gstreamer init");

        let events: Box<Mutex<Events>> = Box::default();
        let user_ptr = Box::into_raw(events);
        let output = Output::from_callbacks(frame_abi::frame_host_callbacks {
            user: user_ptr.cast(),
            surface: Some(on_surface),
            surface_region: Some(on_region),
            surface_removed: Some(on_removed),
            video: Some(on_video),
            message: Some(on_message),
        });
        let shared = Arc::new(Shared::default());
        let (manager, handle) = VirtualManager::start(output.clone(), shared.clone());
        let mut guard = CleanupGuard {
            output: Some(output.clone()),
            manager: Some(manager),
            handle: Some(handle),
            user_ptr,
        };
        let manager_ref = guard.manager.as_ref().unwrap();
        assert!(
            manager_ref.capabilities().supported,
            "{:?}",
            manager_ref.capabilities()
        );

        let events_ref = unsafe { &*user_ptr };
        manager_ref.send(Command::Create {
            source: VirtualTargetKind::Blank {
                width: 1280,
                height: 720,
            },
            size: [1280, 720],
        });

        let deadline = Instant::now() + Duration::from_secs(6);
        let mut created_surface = None;
        while Instant::now() < deadline {
            let guard = events_ref.lock().unwrap();
            let panel_surface = guard.messages.iter().rev().find_map(|msg| match msg {
                HostMessage::VirtualState(s) => s.panels.first().map(|p| p.surface),
                _ => None,
            });
            if let Some(id) = panel_surface
                && guard.surfaces.contains(&id)
                && guard.videos.contains(&id)
            {
                created_surface = Some(id);
                break;
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(50));
        }
        let surface_id = created_surface.expect("virtual panel created and streamed");

        let before = backend.list_monitors().unwrap();
        let panel_before = before.iter().find(|m| m.name == "FRAME-V1").unwrap();
        manager_ref.send(Command::SyncLayout {
            logical_sizes: vec![(surface_id, [1280, 720])],
            placements: vec![PanelLayoutTarget {
                surface: surface_id,
                x: anchor.logical_pos.0,
                y: anchor.logical_pos.1,
            }],
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !lock(events_ref).messages.iter().any(|msg| {
            matches!(msg, HostMessage::Error(err) if err.contains("would overlap"))
        }) {
            assert!(
                Instant::now() < deadline,
                "expected the overlapping layout to be rejected"
            );
            std::thread::sleep(Duration::from_millis(30));
        }
        let after = backend.list_monitors().unwrap();
        let panel_after = after.iter().find(|m| m.name == "FRAME-V1").unwrap();
        assert_eq!(panel_before.logical_pos, panel_after.logical_pos);

        manager_ref.send(Command::SyncLayout {
            logical_sizes: vec![(surface_id, [1280, 720])],
            placements: vec![PanelLayoutTarget {
                surface: surface_id,
                x: 0,
                y: -720,
            }],
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut moved = false;
        while Instant::now() < deadline {
            let guard = events_ref.lock().unwrap();
            if guard.messages.iter().rev().any(|msg| match msg {
                HostMessage::VirtualState(s) => s
                    .panels
                    .iter()
                    .any(|p| p.surface == surface_id && p.logical_pos == [0, -720]),
                _ => false,
            }) {
                moved = true;
                break;
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(
            moved,
            "expected SyncLayout to reposition FRAME-V1 to (0, -720)"
        );

        shared.virtual_resize_ack.store(true, Ordering::Release);
        manager_ref.send(Command::SetSizing {
            surface: surface_id,
            sizing: wayvr_ipc::virtual_panel::Sizing::Resolution([1600, 900]),
        });
        let deadline = Instant::now() + Duration::from_secs(6);
        let resized = loop {
            let events = events_ref.lock().unwrap();
            if lock(&shared.virtual_keyframe_sizes).get(&surface_id) == Some(&(1600, 900))
                && let Some(serial) = events.messages.iter().find_map(|msg| match msg {
                    HostMessage::VerifyVirtualResize {
                        surface,
                        size: [1600, 900],
                        serial,
                    } if *surface == surface_id => Some(*serial),
                    _ => None,
                })
            {
                *lock(&shared.decoded_virtual_resize) = Some((surface_id, serial));
            }
            let panel = events.messages.iter().rev().find_map(|msg| match msg {
                HostMessage::VirtualState(state) => state.panels.first(),
                _ => None,
            });
            if panel.is_some_and(|p| {
                p.surface == surface_id
                    && [p.width, p.height] == [1600, 900]
                    && p.logical_pos == [0, -900]
            }) {
                break true;
            }
            if Instant::now() >= deadline {
                for error in events.messages.iter().filter_map(|msg| match msg {
                    HostMessage::Error(error) => Some(error),
                    _ => None,
                }) {
                    eprintln!("{error}");
                }
                break false;
            }
            drop(events);
            std::thread::sleep(Duration::from_millis(30));
        };
        assert!(
            resized,
            "expected a new keyframe and the same panel at (0, -900)"
        );

        manager_ref.send(Command::SetSizing {
            surface: surface_id,
            sizing: wayvr_ipc::virtual_panel::Sizing::Resolution([900, 1200]),
        });
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let events = events_ref.lock().unwrap();
            if events.messages.iter().any(|msg| {
                matches!(msg,
                    HostMessage::Error(error) if error.contains("Restored the previous resolution")
                )
            }) && lock(&shared.virtual_keyframe_sizes).get(&surface_id) == Some(&(1600, 900))
            {
                let monitors = backend.list_monitors().unwrap();
                let output = monitors.iter().find(|m| m.name == "FRAME-V1").unwrap();
                assert_eq!([output.width, output.height], [1600, 900]);
                assert_eq!(output.logical_pos, (0, -900));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected rollback after missing decode acknowledgement"
            );
            drop(events);
            std::thread::sleep(Duration::from_millis(30));
        }
        shared.virtual_resize_ack.store(false, Ordering::Release);

        manager_ref.send(Command::Close {
            surface: surface_id,
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut closed = false;
        while Instant::now() < deadline {
            let guard = events_ref.lock().unwrap();
            let gone_in_state = guard
                .messages
                .iter()
                .rev()
                .find_map(|msg| match msg {
                    HostMessage::VirtualState(s) => Some(s.panels.is_empty()),
                    _ => None,
                })
                .unwrap_or(false);
            if gone_in_state && guard.removed.contains(&surface_id) {
                closed = true;
                break;
            }
            drop(guard);
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(closed, "expected virtual panel close and removal");

        if std::env::var_os("FRAME_TEST_DETACH").is_some() {
            use wayvr_ipc::virtual_panel::{DetachGesture, DropEdge};
            let mut terminal = std::process::Command::new("alacritty")
                .args([
                    "--class",
                    "frame-detach-test",
                    "--config-file",
                    "/dev/null",
                    "-e",
                    "sleep",
                    "25",
                ])
                .spawn()
                .expect("detach test window");
            let deadline = Instant::now() + Duration::from_secs(5);
            let original = loop {
                if let Some(w) = backend
                    .list_windows()
                    .unwrap()
                    .into_iter()
                    .find(|w| w.app_id == "frame-detach-test")
                {
                    break w;
                }
                assert!(
                    Instant::now() < deadline,
                    "detach test window never appeared"
                );
                std::thread::sleep(Duration::from_millis(25));
            };
            let address = &original.address;
            assert!(!original.floating);
            let (x, y) = anchor.logical_pos;
            let start = [x + 160, y + 100];
            let edge = [x + 1, y + 100];
            for id in [1, 2, 3] {
                backend.set_window_floating(address, true).unwrap();
                backend.resize_floating_window(address, [560, 390]).unwrap();
                backend
                    .move_floating_window(address, [x + 100, y + 40])
                    .unwrap();
                backend.move_cursor(start).unwrap();
                let before_drag = backend.list_windows().unwrap();
                manager_ref.send(Command::Detach(DetachGesture::Begin {
                    id,
                    address: address.clone(),
                    cursor: start,
                }));
                std::thread::sleep(Duration::from_millis(200));
                backend
                    .move_floating_window(address, [x - 50, y + 40])
                    .unwrap();
                backend.move_cursor(edge).unwrap();
                let deadline = Instant::now() + Duration::from_secs(3);
                let preview = loop {
                    let events = lock(events_ref);
                    let latest = events.messages.iter().rev().find_map(|event| match event {
                        HostMessage::DetachPreview(preview) => Some(preview.clone()),
                        _ => None,
                    });
                    if let Some(Some(preview)) = latest {
                        break preview;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "detach preview never armed in gesture {id}: {before_drag:?} -> {:?}, pointer {:?}",
                        backend.list_windows(),
                        backend.cursor_position(),
                    );
                    drop(events);
                    std::thread::sleep(Duration::from_millis(25));
                };
                assert_eq!(preview.edge, DropEdge::Left);
                assert_eq!(
                    backend.list_monitors().unwrap().len(),
                    1,
                    "holding at the edge must not detach"
                );
                if id == 1 {
                    backend.move_cursor(start).unwrap();
                    manager_ref.send(Command::Detach(DetachGesture::Release {
                        id,
                        cursor: start,
                    }));
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !matches!(
                        lock(events_ref)
                            .messages
                            .iter()
                            .rev()
                            .find(|event| matches!(event, HostMessage::DetachPreview(_))),
                        Some(HostMessage::DetachPreview(None))
                    ) {
                        assert!(Instant::now() < deadline, "detach did not cancel");
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    assert_eq!(backend.list_monitors().unwrap().len(), 1);
                    continue;
                }
                // Hyprland retiles a tiled window before dispatching the release.
                backend.set_window_floating(address, false).unwrap();
                shared.virtual_resize_ack.store(id == 3, Ordering::Release);
                let release = if id == 2 {
                    // Retiling changed the position and size. Keep the drop armed
                    // through that change and a small pointer retreat.
                    let near_edge = [x + 30, edge[1]];
                    backend.move_cursor(near_edge).unwrap();
                    std::thread::sleep(Duration::from_millis(125));
                    near_edge
                } else {
                    edge
                };
                manager_ref.send(Command::Detach(DetachGesture::Release {
                    id,
                    cursor: release,
                }));
                if id == 3 {
                    let deadline = Instant::now() + Duration::from_secs(6);
                    while !lock(events_ref).messages.iter().any(|event| {
                        matches!(event, HostMessage::Error(error) if error.contains("Could not stream the detached window"))
                    }) {
                        assert!(Instant::now() < deadline, "detach did not roll back when the headset failed to confirm the first frame");
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    assert_eq!(backend.list_monitors().unwrap().len(), 1);
                    let returned = backend
                        .list_windows()
                        .unwrap()
                        .into_iter()
                        .find(|window| &window.address == address)
                        .unwrap();
                    assert_eq!(returned.workspace_id, original.workspace_id);
                    assert_eq!(returned.monitor_name, original.monitor_name);
                    assert!(!returned.floating);
                    continue;
                }
                let deadline = Instant::now() + Duration::from_secs(6);
                let surface = loop {
                    let events = lock(events_ref);
                    if let Some(surface) =
                        events.messages.iter().rev().find_map(|event| match event {
                            HostMessage::DetachedPanel { surface, placement }
                                if placement == &preview && events.videos.contains(surface) =>
                            {
                                Some(*surface)
                            }
                            _ => None,
                        })
                    {
                        break surface;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "detached output did not start: {:?}",
                        events
                            .messages
                            .iter()
                            .filter_map(|event| match event {
                                HostMessage::Error(error) => Some(error),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                    );
                    drop(events);
                    std::thread::sleep(Duration::from_millis(25));
                };
                let monitors = backend.list_monitors().unwrap();
                let detached = monitors
                    .iter()
                    .find(|m| capture::is_virtual_output_name(&m.name))
                    .unwrap();
                assert_eq!([detached.width, detached.height], preview.size);
                assert_eq!(detached.logical_pos.0 + detached.logical_size.0, x);
                assert!(backend.list_windows().unwrap().iter().any(|window| {
                    &window.address == address
                        && window.floating
                        && window.monitor_name == detached.name
                }));
                assert_eq!(preview.size, [640, 480]);
                if let Some(helper) = std::env::var_os("FRAME_TEST_WINDOW_DRAG") {
                    shared.virtual_window_follow.store(true, Ordering::Release);
                    // Let the initial fit settle before measuring the starting position.
                    std::thread::sleep(Duration::from_millis(1100));
                    let origin = backend
                        .list_windows()
                        .unwrap()
                        .into_iter()
                        .find(|window| &window.address == address)
                        .unwrap();
                    backend
                        .move_cursor([origin.pos[0] + 110, origin.pos[1] + 100])
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(300));
                    assert!(
                        std::process::Command::new(helper)
                            .arg("120")
                            .status()
                            .unwrap()
                            .success()
                    );
                    let deadline = Instant::now() + Duration::from_secs(4);
                    loop {
                        let events = lock(events_ref);
                        if events.messages.iter().any(|event| {
                            matches!(event, HostMessage::VirtualWindowMoved {
                                surface: moved, delta: [dx, dy], output_size: [640, 480],
                            } if *moved == surface && *dx > 0 && dy.abs() < 35)
                        }) {
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "drag did not move the detached panel"
                        );
                        drop(events);
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    let returned = backend
                        .list_windows()
                        .unwrap()
                        .into_iter()
                        .find(|window| &window.address == address)
                        .unwrap();
                    assert_eq!(returned.workspace_id, origin.workspace_id);
                    assert_eq!(returned.pos, origin.pos, "{origin:?} -> {returned:?}");
                }
                manager_ref.send(Command::Close { surface });
                let deadline = Instant::now() + Duration::from_secs(3);
                while !lock(events_ref).removed.contains(&surface)
                    || backend.list_monitors().unwrap().len() != 1
                {
                    assert!(Instant::now() < deadline, "detached output did not close");
                    std::thread::sleep(Duration::from_millis(25));
                }
                assert!(backend.list_windows().unwrap().iter().any(|window| {
                    &window.address == address
                        && !window.floating
                        && window.workspace_id == original.workspace_id
                }));
            }
            let _ = terminal.kill();
            let _ = terminal.wait();
            return;
        }

        if std::env::var_os("FRAME_TEST_WINDOW_FIT").is_some() {
            let output = std::process::Command::new("alacritty")
                .args([
                    "--class",
                    "frame-fit-test",
                    "--config-file",
                    "/dev/null",
                    "-e",
                    "sleep",
                    "30",
                ])
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("Alacritty test window");
            struct TestWindow(std::process::Child);
            impl Drop for TestWindow {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let _window = TestWindow(output);
            let deadline = Instant::now() + Duration::from_secs(5);
            let window = loop {
                if let Some(window) = backend
                    .list_windows()
                    .unwrap()
                    .into_iter()
                    .find(|w| w.app_id == "frame-fit-test")
                {
                    break window;
                }
                assert!(Instant::now() < deadline, "test window never mapped");
                std::thread::sleep(Duration::from_millis(25));
            };
            let last_surface = lock(events_ref).surfaces.iter().copied().max().unwrap();
            manager_ref.send(Command::Create {
                source: VirtualTargetKind::Window {
                    address: window.address.clone(),
                    title: window.title,
                    app_id: window.app_id,
                },
                size: [1280, 720],
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            let panel = loop {
                let events = events_ref.lock().unwrap();
                if let Some(panel) = events
                    .messages
                    .iter()
                    .rev()
                    .find_map(|msg| match msg {
                        HostMessage::VirtualState(state) => state.panels.first().cloned(),
                        _ => None,
                    })
                    .filter(|p| p.surface > last_surface && events.videos.contains(&p.surface))
                {
                    break panel;
                }
                assert!(Instant::now() < deadline, "window panel never streamed");
                drop(events);
                std::thread::sleep(Duration::from_millis(25));
            };
            assert_eq!(panel.fit_to_window, Some(true));
            let address = window.address;
            for cmd in [
                format!("hl.dsp.window.float({{ window = 'address:{address}', action = 'set' }})"),
                format!(
                    "hl.dsp.window.resize({{ window = 'address:{address}', x = 906, y = 604, relative = false }})"
                ),
            ] {
                let output = std::process::Command::new("hyprctl")
                    .args(["dispatch", &cmd])
                    .output()
                    .unwrap();
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
            }
            let deadline = Instant::now() + Duration::from_secs(6);
            let fitted = loop {
                let events = events_ref.lock().unwrap();
                let latest = events.messages.iter().rev().find_map(|msg| match msg {
                    HostMessage::VirtualState(state) => {
                        state.panels.iter().find(|p| p.surface == panel.surface)
                    }
                    _ => None,
                });
                if let Some(p) = latest.filter(|p| p.fit_status.is_none() && p.width != 1280) {
                    let app = backend
                        .list_windows()
                        .unwrap()
                        .into_iter()
                        .find(|w| w.address == address)
                        .unwrap();
                    let border = backend.window_border().unwrap();
                    assert_eq!(
                        [p.width, p.height],
                        app.size.map(|size| (size + 2 * border + 1) & !1)
                    );
                    assert_eq!(app.pos, p.logical_pos.map(|axis| axis + border as i32));
                    assert!(events.videos.contains(&p.surface));
                    break [p.width, p.height];
                }
                assert!(
                    Instant::now() < deadline,
                    "window fit did not settle: {latest:?}"
                );
                drop(events);
                std::thread::sleep(Duration::from_millis(25));
            };
            std::thread::sleep(Duration::from_millis(650));
            let cmd = format!(
                "hl.dsp.window.resize({{ window = 'address:{address}', x = 200, y = 0, relative = true }})"
            );
            let reply = std::process::Command::new("hyprctl")
                .args(["dispatch", &cmd])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&reply.stdout).trim(), "ok");
            let deadline = Instant::now() + Duration::from_secs(6);
            loop {
                let events = lock(events_ref);
                let latest = events.messages.iter().rev().find_map(|msg| match msg {
                    HostMessage::VirtualState(state) => {
                        state.panels.iter().find(|p| p.surface == panel.surface)
                    }
                    _ => None,
                });
                if let Some(p) = latest.filter(|p| p.width == fitted[0] + 200) {
                    assert_eq!(p.height, fitted[1]);
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "width-only fit did not settle: {latest:?}"
                );
                drop(events);
                std::thread::sleep(Duration::from_millis(25));
            }

            if let Some(helper) = std::env::var_os("FRAME_TEST_WINDOW_DRAG") {
                shared.virtual_window_follow.store(true, Ordering::Release);
                for (index, delta) in [120_i32, -260].into_iter().enumerate() {
                    let origin = backend
                        .list_windows()
                        .unwrap()
                        .into_iter()
                        .find(|w| w.address == address)
                        .unwrap();
                    backend
                        .move_cursor([origin.pos[0] + 110, origin.pos[1] + 100])
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(800));
                    assert!(
                        std::process::Command::new(&helper)
                            .arg(delta.to_string())
                            .status()
                            .unwrap()
                            .success()
                    );
                    let deadline = Instant::now() + Duration::from_secs(4);
                    loop {
                        let events = lock(events_ref);
                        let movement = events
                            .messages
                            .iter()
                            .filter_map(|msg| match msg {
                                HostMessage::VirtualWindowMoved { surface, delta, .. }
                                    if *surface == panel.surface =>
                                {
                                    Some(*delta)
                                }
                                _ => None,
                            })
                            .nth(index);
                        if let Some([dx, dy]) = movement {
                            assert_eq!(dx.signum(), delta.signum());
                            assert!(dy.abs() < 35);
                            break;
                        }
                        assert!(
                            Instant::now() < deadline,
                            "window drag ({delta}) did not move the panel"
                        );
                        drop(events);
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    let returned = backend
                        .list_windows()
                        .unwrap()
                        .into_iter()
                        .find(|w| w.address == address)
                        .unwrap();
                    assert_eq!(returned.workspace_id, origin.workspace_id);
                    assert_eq!(returned.pos, origin.pos);
                }
            }
            manager_ref.send(Command::Close {
                surface: panel.surface,
            });
        }

        for target in [
            source.clone(),
            VirtualTargetKind::Blank {
                width: 1280,
                height: 720,
            },
            VirtualTargetKind::Blank {
                width: 1280,
                height: 720,
            },
        ] {
            manager_ref.send(Command::Create {
                source: target,
                size: [1280, 720],
            });
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut workspace_panel = None;
        while Instant::now() < deadline {
            let events = events_ref.lock().unwrap();
            let panels = events.messages.iter().rev().find_map(|msg| match msg {
                HostMessage::VirtualState(state) => Some(&state.panels),
                _ => None,
            });
            if let Some(panels) = panels
                && panels.len() == 3
                && panels.iter().all(|panel| events.videos.contains(&panel.surface))
            {
                workspace_panel = Some(panels[0].surface);
                break;
            }
            drop(events);
            std::thread::sleep(Duration::from_millis(30));
        }
        let workspace_panel = workspace_panel.expect("three rapidly requested panels streamed");
        manager_ref.send(Command::Create { source, size: [1280, 720] });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut revealed = false;
        while Instant::now() < deadline {
            revealed = events_ref.lock().unwrap().messages.iter().any(|msg| {
                matches!(msg, HostMessage::RevealVirtualPanel(surface) if *surface == workspace_panel)
            });
            if revealed {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        assert!(revealed, "an existing workspace panel should reopen at the panel limit");

        let (other_panels, other_announcements) = {
            let events = events_ref.lock().unwrap();
            let panels = events
                .messages
                .iter()
                .rev()
                .find_map(|msg| match msg {
                    HostMessage::VirtualState(state) => Some(&state.panels),
                    _ => None,
                })
                .unwrap();
            let other_panels: BTreeMap<_, _> = panels
                .iter()
                .filter(|p| p.surface != workspace_panel)
                .map(|p| (p.surface, [p.width, p.height]))
                .collect();
            let other_announcements = events
                .surfaces
                .iter()
                .filter(|id| other_panels.contains_key(id))
                .count();
            (other_panels, other_announcements)
        };
        manager_ref.send(Command::SetSizing {
            surface: workspace_panel,
            sizing: wayvr_ipc::virtual_panel::Sizing::Resolution([1600, 900]),
        });
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let events = events_ref.lock().unwrap();
            let panels = events
                .messages
                .iter()
                .rev()
                .find_map(|msg| match msg {
                    HostMessage::VirtualState(state) => Some(&state.panels),
                    _ => None,
                })
                .unwrap();
            if panels
                .iter()
                .any(|p| p.surface == workspace_panel && [p.width, p.height] == [1600, 900])
            {
                assert_eq!(panels.len(), 3);
                for p in panels.iter().filter(|p| p.surface != workspace_panel) {
                    assert_eq!(other_panels[&p.surface], [p.width, p.height]);
                }
                assert_eq!(
                    events
                        .surfaces
                        .iter()
                        .filter(|id| other_panels.contains_key(id))
                        .count(),
                    other_announcements
                );
                assert!(
                    events
                        .removed
                        .iter()
                        .all(|id| !other_panels.contains_key(id))
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected to resize one of three streaming panels, actual {panels:?}, errors: {:?}",
                events.messages.iter().filter_map(|msg| match msg {
                    HostMessage::Error(err) => Some(err),
                    _ => None,
                }).collect::<Vec<_>>()
            );
            drop(events);
            std::thread::sleep(Duration::from_millis(30));
        }

        guard.output.take().unwrap().stop();
        guard.manager.take().unwrap().send(Command::Shutdown);
        guard.handle.take().unwrap().join().expect("join virtual worker");
        let remaining = backend.list_monitors().unwrap();
        assert!(remaining.iter().all(|m| !capture::is_virtual_output_name(&m.name)));
        assert!(backend.list_workspaces().unwrap().iter().any(|ws| {
            ws.id == workspace_id && ws.monitor_name == anchor.name
        }), "the shared workspace should return to its original monitor");
        drop(guard);
    }
}
