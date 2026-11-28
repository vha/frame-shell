use std::{
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use smol::channel::Sender;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm,
        wl_shm_pool, wl_surface,
    },
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{zwp_locked_pointer_v1, zwp_pointer_constraints_v1},
    relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1},
    viewporter::client::{wp_viewport, wp_viewporter},
};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use wayvr_ipc::virtual_panel::Request;

type Constraints = zwp_pointer_constraints_v1::ZwpPointerConstraintsV1;

#[derive(Default)]
struct Capture {
    outputs: Vec<(wl_output::WlOutput, String)>,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    viewport: Option<wp_viewport::WpViewport>,
    constraints: Option<Constraints>,
    lock: Option<zwp_locked_pointer_v1::ZwpLockedPointerV1>,
    relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    relative: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_focus: bool,
    pointer_enter: Option<u32>,
    locked: bool,
    left_down: bool,
    shifts: [bool; 2],
    ctrls: [bool; 2],
    pending: [f64; 3],
    scale_steps: f64,
    cancel: Option<Instant>,
    completed: Option<bool>,
}

impl Capture {
    fn precision(&self) -> f64 {
        if self.ctrls.iter().any(|held| *held) {
            0.25
        } else {
            1.0
        }
    }

    fn cancel(&mut self) {
        if self.left_down {
            self.cancel.get_or_insert_with(Instant::now);
        } else {
            self.completed = Some(false);
        }
    }
}

macro_rules! ignore_events {
    ($($proxy:path),+ $(,)?) => {$(
        impl Dispatch<$proxy, ()> for Capture {
            fn event(_: &mut Self, _: &$proxy, _: <$proxy as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_surface::WlSurface,
    wl_buffer::WlBuffer,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    Constraints,
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Capture {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::GlobalRemove { name } = event
            && state
                .outputs
                .iter()
                .any(|(out, _)| out.data::<u32>() == Some(&name))
        {
            state.cancel();
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for Capture {
    fn event(
        state: &mut Self,
        proxy: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs.push((proxy.clone(), name));
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for Capture {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                if width == 0 || height == 0 {
                    state.cancel();
                    return;
                }
                if let (Some(surface), Some(buffer), Some(viewport)) =
                    (&state.surface, &state.buffer, &state.viewport)
                {
                    viewport.set_destination(width as i32, height as i32);
                    surface.attach(Some(buffer), 0, 0);
                    surface.damage(0, 0, width as i32, height as i32);
                    surface.commit();
                }
            }
            zwlr_layer_surface_v1::Event::Closed => state.cancel(),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for Capture {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        if caps.contains(wl_seat::Capability::Pointer) {
            if state.pointer.is_none() {
                let pointer = seat.get_pointer(qh, ());
                state.relative = state
                    .relative_manager
                    .as_ref()
                    .map(|manager| manager.get_relative_pointer(&pointer, qh, ()));
                state.pointer = Some(pointer);
            }
        } else if state.pointer.is_some() {
            state.cancel();
        }
        if caps.contains(wl_seat::Capability::Keyboard) {
            if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        } else if state.keyboard.is_some() {
            state.cancel();
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Enter { keys, .. } => {
                state.keyboard_focus = true;
                state.shifts = [false; 2];
                state.ctrls = [false; 2];
                for key in keys.chunks_exact(4) {
                    match u32::from_ne_bytes(key.try_into().unwrap()) {
                        42 => state.shifts[0] = true,
                        54 => state.shifts[1] = true,
                        29 => state.ctrls[0] = true,
                        97 => state.ctrls[1] = true,
                        _ => {}
                    }
                }
            }
            wl_keyboard::Event::Leave { .. } if state.keyboard_focus => state.cancel(),
            wl_keyboard::Event::Key {
                key,
                state: WEnum::Value(key_state),
                ..
            } => {
                let down = key_state == wl_keyboard::KeyState::Pressed;
                match key {
                    1 if down => state.cancel(), // Escape
                    42 => state.shifts[0] = down,
                    54 => state.shifts[1] = down,
                    29 => state.ctrls[0] = down,
                    97 => state.ctrls[1] = down,
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Capture {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. } => state.pointer_enter = Some(serial),
            wl_pointer::Event::Leave { .. } => {
                if state.pointer_enter.take().is_some() {
                    state.cancel();
                }
            }
            wl_pointer::Event::Button {
                button: 272,
                state: WEnum::Value(button),
                ..
            } => match button {
                wl_pointer::ButtonState::Pressed
                    if state.keyboard_focus
                        && !state.left_down
                        && let (Some(constraints), Some(surface)) =
                            (&state.constraints, &state.surface) =>
                {
                    state.left_down = true;
                    state.lock = Some(constraints.lock_pointer(
                        surface,
                        pointer,
                        None,
                        zwp_pointer_constraints_v1::Lifetime::Oneshot,
                        qh,
                        (),
                    ));
                }
                wl_pointer::ButtonState::Released if state.left_down => {
                    state.left_down = false;
                    state.completed = Some(state.cancel.is_none() && state.locked);
                }
                _ => {}
            },
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } if state.locked && state.cancel.is_none() && state.completed.is_none() => {
                let value = value * state.precision();
                if state.shifts.iter().any(|held| *held) {
                    state.scale_steps -= value / 15.0;
                } else {
                    state.pending[2] -= value * 0.002;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &zwp_locked_pointer_v1::ZwpLockedPointerV1,
        event: zwp_locked_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => {
                state.locked = true;
                if let (Some(pointer), Some(serial)) = (&state.pointer, state.pointer_enter) {
                    pointer.set_cursor(serial, None, 0, 0);
                }
            }
            zwp_locked_pointer_v1::Event::Unlocked if state.completed.is_none() => state.cancel(),
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for Capture {
    fn event(
        state: &mut Self,
        _: &zwp_relative_pointer_v1::ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event
            && state.locked
            && state.cancel.is_none()
            && state.completed.is_none()
        {
            let precision = state.precision();
            state.pending[0] += dx * precision;
            state.pending[1] -= dy * precision;
        }
    }
}

pub fn run(output: &str, gain: f64, sender: &Sender<Request>) -> Result<bool> {
    let conn = Connection::connect_to_env().context("Connect to the PC's Wayland session")?;
    let (globals, mut queue) = registry_queue_init::<Capture>(&conn)?;
    let qh = queue.handle();
    let mut state = Capture::default();

    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ())?;
    let shm: wl_shm::WlShm = globals.bind(&qh, 1..=1, ())?;
    let layer: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("The compositor needs layer-shell for mouse dragging")?;
    let viewporter: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ())?;
    state.constraints = Some(
        globals
            .bind(&qh, 1..=1, ())
            .context("The compositor needs pointer constraints")?,
    );
    state.relative_manager = Some(
        globals
            .bind(&qh, 1..=1, ())
            .context("The compositor needs relative pointer motion")?,
    );
    let _seat: wl_seat::WlSeat = globals.bind(&qh, 1..=8, ())?;
    for global in globals.contents().clone_list() {
        if global.interface == wl_output::WlOutput::interface().name && global.version >= 4 {
            let _: wl_output::WlOutput = globals.registry().bind(global.name, 4, &qh, global.name);
        }
    }
    queue.roundtrip(&mut state)?;
    let output = &state
        .outputs
        .iter()
        .find(|(_, name)| name == output)
        .context("The output under the cursor disappeared")?
        .0;
    let surface = compositor.create_surface(&qh, ());
    let layer = layer.get_layer_surface(
        &surface,
        Some(output),
        zwlr_layer_shell_v1::Layer::Overlay,
        "frame-shell-panel-drag".into(),
        &qh,
        (),
    );
    use zwlr_layer_surface_v1::{Anchor, KeyboardInteractivity};
    layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    let fd = unsafe { libc::memfd_create(c"frame-panel-drag".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(fd >= 0, "Allocate transparent pointer surface");
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    ensure!(
        unsafe { libc::ftruncate(fd.as_raw_fd(), 4) } == 0,
        "Size transparent pointer surface"
    );
    let pool = shm.create_pool(fd.as_fd(), 4, &qh, ());
    state.buffer = Some(pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, &qh, ()));
    pool.destroy();
    state.viewport = Some(viewporter.get_viewport(&surface, &qh, ()));
    state.surface = Some(surface);
    state.layer = Some(layer);
    state.surface.as_ref().unwrap().commit();

    let mut last_send = Instant::now();
    let start = last_send;
    let mut last_activity = start;
    loop {
        conn.flush()?;
        queue.dispatch_pending(&mut state)?;
        if sender.is_closed() {
            return Ok(false);
        }
        if state.pending != [0.0; 3] || state.scale_steps != 0.0 {
            last_activity = Instant::now();
        }
        if state
            .cancel
            .is_some_and(|time| time.elapsed() >= Duration::from_secs(2))
            || (state.lock.is_none() && start.elapsed() > Duration::from_secs(10))
            || last_activity.elapsed() > Duration::from_secs(30)
        {
            return Ok(false);
        }
        if state.cancel.is_none()
            && (state.completed.is_some() || last_send.elapsed() >= Duration::from_millis(16))
        {
            let [x, y, z] = std::mem::take(&mut state.pending);
            let scale = std::mem::take(&mut state.scale_steps);
            if [x, y, z] != [0.0; 3]
                || scale != 0.0
                || last_send.elapsed() >= Duration::from_millis(500)
            {
                let _ = sender.send_blocking(Request::DragMotion {
                    translation: [x * gain, y * gain, z].map(|v| v.clamp(-0.5, 0.5) as f32),
                    scale_steps: scale.clamp(-6.0, 6.0) as f32,
                });
                last_send = Instant::now();
            }
        }
        if let Some(commit) = state.completed {
            return Ok(commit);
        }
        let Some(read) = queue.prepare_read() else {
            continue;
        };
        let mut poll = libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut poll, 1, 12) };
        if n > 0 {
            read.read()?;
        } else if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::Interrupted {
                return Err(err.into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::thread;
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };

    ignore_events!(ZwlrVirtualPointerManagerV1, ZwlrVirtualPointerV1);

    #[test]
    #[ignore = "requires an isolated Hyprland session with a test window and synthetic input"]
    fn live_drag_captures_clicks_and_escape() {
        assert!(std::env::var_os("FRAME_TEST_NESTED_DRAG").is_some());
        let conn = Connection::connect_to_env().unwrap();
        let (globals, mut queue) = registry_queue_init::<Capture>(&conn).unwrap();
        let qh = queue.handle();
        let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 2..=2, ()).unwrap();
        let pointer = manager.create_virtual_pointer(None, &qh, ());
        let mut state = Capture::default();
        queue.roundtrip(&mut state).unwrap();
        let output = std::env::var("FRAME_TEST_NESTED_DRAG").unwrap();

        let buttons = |down| {
            use wl_pointer::ButtonState;
            let state = if down {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            };
            pointer.button(1, 272, state);
            pointer.frame();
            conn.flush().unwrap();
        };
        pointer.motion_absolute(1, 500, 500, 1000, 1000);
        pointer.frame();
        conn.flush().unwrap();
        thread::sleep(Duration::from_millis(100));

        for cancel in [false, true] {
            let (tx, rx) = smol::channel::unbounded();
            let output = output.clone();
            let capture = thread::spawn(move || run(&output, 0.0015, &tx));
            smol::block_on(smol::future::race(
                async {
                    rx.recv().await.expect("capture closed before arming");
                },
                async {
                    smol::Timer::after(Duration::from_secs(2)).await;
                    panic!("capture did not arm");
                },
            ));
            buttons(true);
            thread::sleep(Duration::from_millis(100));
            // Move farther than the distance from the center of the nested
            // output to its edge while the cursor is locked in place.
            for step in 0..24 {
                pointer.motion(10 + step * 25, 100.0, -4.0);
                pointer.frame();
                conn.flush().unwrap();
                thread::sleep(Duration::from_millis(25));
            }
            if cancel {
                assert!(
                    Command::new("wtype")
                        .args(["-k", "Escape"])
                        .stdout(Stdio::null())
                        .status()
                        .unwrap()
                        .success()
                );
                thread::sleep(Duration::from_millis(80));
            }
            buttons(false);
            assert_eq!(capture.join().unwrap().unwrap(), !cancel);
            let [dx, dy] =
                std::iter::from_fn(|| rx.try_recv().ok()).fold([0.0, 0.0], |[x, y], request| {
                    match request {
                        Request::DragMotion {
                            translation: [dx, dy, _],
                            ..
                        } => [x + dx, y + dy],
                        _ => [x, y],
                    }
                });
            assert!(dx > 3.0 && dy > 0.0, "long drag delivered [{dx}, {dy}]");
        }

        // A normal click after teardown must go to the underlying application.
        thread::sleep(Duration::from_millis(100));
        buttons(true);
        buttons(false);
        thread::sleep(Duration::from_millis(150));
    }
}
