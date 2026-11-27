# PRD — Keyboard and mouse control for the VR desktop

**Status** · Implemented in the development builds. Steam Frame acceptance remains open.

**Target** · Frame Shell on Steam Frame with a Hyprland PC host  
**Related** · [Virtual PC panels and layout sync](../docs/pc-virtual-panels-prd.md)

## 1. Overview

A user can already move a physical mouse onto a virtual output and use the app there. Moving the VR panel itself still uses WayVR's grab interaction. Window-backed panels also stream the entire virtual monitor, so resizing a floating window can leave a large area of desktop background around it. The keyboard bar and dashboard offer presets for blank outputs.

The goal is to arrange and manage the VR desktop with the same keyboard and mouse used on the PC. Use a configurable chord plus left-drag to move a VR panel, keep ordinary Hyprland window moves available, and choose a custom resolution for a virtual output. Window-backed panels should also have a view that follows the app's bounds.

Custom dimensions at creation are a small extension of the current picker. Live output resize and automatic window fitting require coordinated host and headset changes. Reliable PC-side drag capture is the largest unknown because it crosses the compositor, the PC–headset link and WayVR's input and layout state.

The first release prioritizes the keyboard and mouse connected to the PC. Eligible window-backed panels default to `Fit to window`, which matches the dedicated output to the app's settled size. `Fixed resolution` is available per panel.

## 2. Goals

- Arrange panels and manage their lifecycle without picking up a controller or taking off the headset.
- Keep normal app clicks, typing and existing Hyprland shortcuts available outside an explicit panel gesture.
- Keep the VR arrangement and physical mouse edge crossing consistent after a move.
- Offer app-sized presentation and independently configurable virtual output resolutions.
- Keep unaffected streams operating through all panel edits and recover cleanly from rejected modes and interrupted gestures.

## 3. User stories

### US-1 — Move and scale a panel with the PC mouse

As a user, I want a separate chord for VR panel moves so I can keep my usual Hyprland window shortcuts.

- [ ] With the PC cursor on a visible virtual output, a configurable chord plus left-drag moves its VR panel. For example, `Super+Shift+Left Drag` is a suggested binding to check for conflicts, and the starting panel stays selected throughout the drag.
- [ ] With the cursor on a physical screen or outside an eligible panel, a keyboard-accessible panel list allows explicit selection, including panels that are difficult to reach via mouse edges.
- [ ] Drag maps horizontal and vertical motion to a plane at the panel's starting depth. A separate wheel or keyboard action adjusts depth or apparent scale with a fine adjustment option.
- [ ] Button release commits. Escape restores the starting pose and returns normal input. The app receives zero gesture button presses or releases. Test with Hyprland's ordinary `Super+drag` binding enabled.
- [ ] With Auto-sync enabled, the compositor layout updates once the gesture settles. Dragging a panel above a physical screen leaves an edge the mouse can cross. With Auto-sync disabled, the headset pose changes and the compositor keeps the previous placement.

### US-2 — Manage panels from the keyboard

As a user, I want to open, find and recover panels even when the pointer cannot reach them.

- [ ] A keyboard-invoked list shows the connected PC and its blank, window and workspace targets. Arrow keys navigate, Enter selects and Escape exits.
- [ ] Active panels offer show or hide, focus, reset position and close. Closing a panel follows existing app and workspace restoration rules. A hidden shared target can be revealed and focused with the keyboard.
- [ ] The flow works while wearing the Steam Frame, including opening a panel, typing into its app, moving it and returning to ordinary desktop use.

### US-3 — Fit the virtual output to a shared app

As a user, I want the real mouse-crossing edges to follow the fitted app so I can move between panels without crossing unseen desktop space.

- [ ] For an eligible single floating window on its dedicated output, resize the virtual monitor after the app size settles. Fit the app's outer bounds within validated mode limits and retain the VR panel's center and orientation.
- [ ] Offer `Fixed resolution` per panel. For tiled, maximized, fullscreen or constrained apps, retain the last working mode when fitting cannot converge. Show the applied mode and the reason when a fit cannot be applied.
- [ ] Test mouse entry and exit through every connected edge after both growing and shrinking the output. Preserve the shared edge with the physical anchor when possible. When the cursor is still inside the resized output, preserve its position relative to the app within supported geometry. If it falls outside, verify that the compositor leaves it reachable and visible on a connected output.
- [ ] Verify native Wayland and XWayland app resizing, popups and click mapping. Confirm that the fitted stream recovers independently and that other streams keep their resolution and continue delivering frames.

### US-4 — Set a custom virtual output resolution

As a user, I want to enter dimensions for a virtual output when the presets do not suit my work.

- [ ] The keyboard-accessible picker accepts width and height in pixels, with the existing presets as shortcuts. An existing panel has an explicit `Display resolution` action.
- [ ] Start with the host's current limits of 640–3840 pixels wide and 480–2160 high, with even dimensions and 60 Hz at scale 1. Report the accepted mode before applying any normalization. Test portrait and wide aspect ratios as well as invalid inputs.
- [ ] A successful resize retains the same panel, pose, workspace and window ownership. It updates input regions and recomputes layout from the compositor's actual logical dimensions.
- [ ] A rejected or failed stream transition keeps or restores the last working mode. The user gets feedback and can retry from the keyboard.

## 4. Functional requirements

1. **FR-1 · Targeting.** Identify panels by the connected host and session surface ID. Resolve the cursor against current virtual output bounds at gesture start. Explicit selection takes precedence when active. Physical monitor overlays keep their role as layout anchors.
2. **FR-2 · Input ownership.** Acquire input only during an explicit editing session. Keep the dragged app from receiving the initiating click. Release capture on commit, cancel, focus loss, device removal, disconnect or timeout. A second controller or mouse gesture cannot take ownership of the same panel mid-drag.
3. **FR-3 · Pose and layout.** WayVR owns the visual pose and updates it during the gesture. The host applies a virtual-only output layout after release through the existing solver. Keep physical monitor positions and scales fixed. Restore normal pointer routing after the compositor confirms new geometry. If repositioning displaced a cursor that began on the moved output, test restoring its output-relative position once.
4. **FR-4 · Separate dimensions.** VR scale changes the apparent size only. Display resolution sets the output mode. Fit to window derives the output mode from the settled app size. Keep the pointer mapping consistent with confirmed output geometry.
5. **FR-5 · Resize transaction.** Validate and serialize output-mode changes with layout changes. Coalesce superseded requests and ignore stale responses. Publish confirmed geometry, deliver the first keyframe at the new size and keep the last displayed image where buffer lifetime permits. If an apply or stream reconfiguration fails, report the outcome and restore the previous working geometry where available.
6. **FR-6 · Compatibility.** Advertise support for new controls independently of basic virtual panels. Keep the existing picker, app input and streaming usable on older peers. Use an opt-in compositor binding and a local command path with no per-frame shell launches, persistent device grabs or changes to existing user shortcuts.
7. **FR-7 · Settings and recovery.** Persist bindings, gain, defaults and optional custom presets. Keep resolved window IDs, active gestures and pending fit state scoped to the active panel and connection. Test native host, Flatpak and an actual Steam Frame.

## 5. Technical approach

### PC keyboard and mouse

The live Hyprland 0.57.2 registry advertises layer-shell, relative pointer and pointer constraints. Hyprland exposes configurable mouse binds and cursor/output queries. Wayland relative motion follows pointer focus, so a global keybind or an IPC call alone cannot deliver a continuous independent drag. The existing headset-to-PC `PcInput` path injects pointer input into the PC, and the WayVR IPC proxy already routes local PC commands to the headset. The proxy has no pose-edit command today. [Hyprland mouse binds](https://wiki.hypr.land/configuring/core/binds/devices/mouse/) · [Hyprland Lua utilities](https://wiki.hypr.land/configuring/core/advanced-configuration/lua-utilities/) · [Wayland relative pointer protocol](https://raw.githubusercontent.com/wayland-mirror/wayland-protocols/main/unstable/relative-pointer/relative-pointer-unstable-v1.xml) · [Shell–host IPC](../ARCHITECTURE.md#shell--host)

Start by exposing a small continuous editing session through IPC. A companion process can send begin, coalesced motion, commit and cancel operations over one connection. WayVR applies movement on the render thread and reuses its grab-release path and approximately 350 ms layout debounce. Build keyboard nudge, depth and scale actions on the same session so the keyboard workflow works independently of mouse capture.

Test a compositor-bound edit mode using a focused input surface and a relative-pointer lock. Arm it before the mouse press if Wayland focus or the app's implicit grab prevents taking ownership during the chord. Compare this with a compositor-side input hook if a single chord cannot provide reliable activation and release. Avoid per-motion synchronous polling of compositor IPC. A temporary capture backend must demonstrate no click-through and reliable release before shipping the chord. Retain the keyboard edit mode even if the chord needs a later iteration. [WayVR grab handling](../wayvr/wayvr/src/backend/input.rs) · [Layout synchronization](../wayvr/wayvr/src/frame_shell/desktops.rs)

For devices paired to the headset, WayVR already has libinput/evdev capture and key-combo handling. If this setup is enabled, route editing actions through the same panel editor. Normal app control on a streamed PC needs its own routing check. The current `PcInput::mouse_move_rel` implementation is empty. [Local device events](../wayvr/wayvr/src/backend/wayvr/input_capture.rs) · [Local keyboard and mouse routing](../wayvr/wayvr/src/backend/wayvr/mod.rs) · [PC desktop overlay input](../wayvr/wayvr/src/frame_shell/desktops.rs)

### Window fitting and mouse crossing

Hyprland routes the PC mouse over the real logical output rectangles. On this host, `j/clients` supplies window position, size and floating/fullscreen state. The backend currently discards those fields, and the public event list lacks a continuous resize feed. Read geometry for active fitted panels at a bounded rate. After a quiet interval, derive a valid mode from the app's outer bounds and recenter it on the resized output. Treat compositor-driven changes caused by the transaction as part of that same transaction to prevent resize loops. [Hyprland IPC events](https://wiki.hypr.land/ipc/) · [Hyprland backend](../desktop/src/compositor/hyprland.rs)

For example, if a virtual output is immediately to the right of a physical monitor and grows from 1200 to 1600 pixels wide, keep its left edge attached to the physical monitor where possible. The entry point stays in the same place and the far edge extends 400 pixels. A neighbor attached at the far edge may need to move. If the output is above or left of its anchor, preserve the touching edge by adjusting its origin when necessary. Check compositor pointer behavior when an origin changes or when a shrink removes the area under the cursor. Keep physical monitor modes and positions fixed. With Auto-sync disabled, prefer keeping the existing 2D attachment and reject a mode that cannot preserve a valid layout. [Layout synchronization](../wayvr/wayvr/src/frame_shell/desktops.rs)

### Custom resolution and live resizing

Existing create messages already carry dimensions, the host already validates them, and the output backend can request a custom mode. Custom size at creation is mostly UI and validation. Resizing an existing panel needs a new command in the serialized virtual panel worker. Use the existing mode transaction and wait for authoritative geometry. Each virtual output has its own capture worker, and the headset receiver recreates the decoder only for the surface whose dimensions change. Global pointer regions can still change when the output rectangle changes. Test whether decoder recreation briefly delays delivery on other streams even though their decoders are preserved. Verify bounded latency and keyframe recovery on the actual headset before enabling repeated automatic resolution changes. To preserve mouse edge crossing, validate the resulting mode and layout together or repair the layout before presenting the resize as complete. [Virtual panel worker](../desktop/src/virtual_panels.rs) · [Wayland output configuration](../desktop/src/compositor/wlr_output.rs) · [Headset decoder lifecycle](../wivrn/client/linux/desktop_receiver.cpp)

## 6. Delivery sequence

1. **Keyboard control and custom creation size** — Add panel selection and lifecycle commands, 3D nudge/depth/scale using existing WayVR transforms, and editable output dimensions at creation. Validate the controller-free workflow on the headset.
2. **PC chord and drag** — Prove capture/focus behavior on Hyprland. Integrate the gesture, preview locally and commit desired layout on release. Add disconnect, cancellation and conflict tests.
3. **Live custom modes** — Implement transactional mode changes. Stress decoder recovery and confirm stream isolation and pointer edge crossing at negative origins and mixed scale.
4. **Fit to window** — Reuse the mode transaction after the window resize settles. Validate fixed-resolution fallback, resizing in both directions and stable mouse crossings.

## 7. Success metrics

- A user creates, selects, moves, scales, hides, restores and closes a panel without reaching for a VR controller. Run the complete workflow on Steam Frame with the PC keyboard and mouse.
- Target a p95 drag-to-visible-motion latency below 50 ms on a healthy LAN. Record from PC motion to headset overlay update and report link/render delay separately.
- Zero click-through, stuck buttons or stuck modifiers in 100 scripted complete, canceled and interrupted gestures. Confirm ordinary focus and shortcuts immediately after release.
- After a settled app resize, fitted content and click mapping agree within two capture pixels at corners and center. For a successful display-mode change, target a first correctly sized frame within one second of compositor confirmation and zero decoder resets on other streams.
- Run three panels with mixed DPI and a negative desktop origin. Exercise hide/show, alternate PC selection, output resize, compositor configuration failure and headset disconnect. Verify pointer edge crossing and collect stream metrics for affected and unaffected panels.

## 8. Scope boundaries and open questions

- The first release targets Hyprland virtual outputs and Steam Frame. Controller-free management covers panel creation, selection, placement, size, visibility and teardown. Cross-PC cursor bridging, arbitrary 3D rotation and remapping regular game inputs are separate work.
- Confirm the preferred binding and whether a one-chord hold-to-drag gesture is required immediately or an explicit edit mode is acceptable for the first release.
- Verify cursor position when a fitted output grows, shrinks or changes origin. Check which window constraints require fixed-resolution fallback.
- Verify single-stream recovery and popup behavior on Steam Frame across repeated output resizes.

## 9. Implementation notes

- The opt-in Hyprland shortcut arms a temporary input surface before the left press. The picker can arm the same editor for an explicitly selected panel. Scroll adjusts depth, Shift+scroll adjusts scale, and Ctrl gives finer adjustment.
- The PC picker exposes fixed resolution and fit status. Real mode changes preserve reachable mouse edges and can move a chain of adjacent virtual outputs. The host rolls the mode back if a compatible headset fails to confirm a decoded frame within the timeout.
- In **Fit to window** mode, a normal Hyprland drag moves the VR panel after the floating window and pointer settle. The panel keeps the pixel density of its first streamed mode when the app resizes.
- Configuration and example shortcuts are in the [development guide](../DEVELOPMENT.md#pc-keyboard-controls-for-virtual-panels).
