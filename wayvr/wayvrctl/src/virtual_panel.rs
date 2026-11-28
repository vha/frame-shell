use std::{
    env, fs,
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use wayvr_ipc::{
    client::WayVRClient,
    virtual_panel::{
        Action, DEFAULT_SIZE, DetachGesture, Direction, HostPanels, Request, Response, SIZE_REQUIREMENTS, Sizing,
        Target, valid_size,
    },
};

use crate::helper::WayVRClientState;

mod capture_drag;

const DEFAULT_DRAG_GAIN: f64 = 0.0015;

#[derive(Debug, clap::Subcommand)]
pub enum CommandLine {
    /// List connected PCs, active panels and available share targets as JSON
    List,
    /// Refresh windows and workspaces on a PC
    Refresh { pc: u32 },
    /// Choose a PC, panel or share target in an interactive terminal picker
    Pick,
    /// Forward the existing Hyprland move gesture to Frame Shell
    Detach {
        #[command(subcommand)]
        gesture: DetachCommand,
    },
    /// Arm mouse drag for the panel under the PC cursor, or an explicitly selected panel
    Drag {
        #[arg(long, requires = "surface")]
        pc: Option<u32>,
        #[arg(long, requires = "pc")]
        surface: Option<u32>,
        #[arg(long)]
        gain: Option<f64>,
    },
    /// Share a blank screen, window or workspace at the given resolution
    Add {
        pc: u32,
        #[arg(long, value_parser = parse_size)]
        size: Option<[u32; 2]>,
        #[command(subcommand)]
        target: ShareTarget,
    },
    /// Reveal a hidden panel
    Show { pc: u32, surface: u32 },
    /// Hide a panel
    Hide { pc: u32, surface: u32 },
    /// Reveal a panel and focus its virtual output on the PC
    Focus { pc: u32, surface: u32 },
    /// Restore a panel's default pose
    Reset { pc: u32, surface: u32 },
    /// Close a panel and return its windows to the PC desktop
    Close { pc: u32, surface: u32 },
    /// Change an active panel's display resolution
    Resize {
        pc: u32,
        surface: u32,
        #[arg(value_parser = parse_size)]
        size: [u32; 2],
    },
    /// Choose whether a window panel follows its app's size
    Fit {
        pc: u32,
        surface: u32,
        #[arg(value_enum)]
        mode: FitMode,
    },
    /// Nudge a visible panel in VR
    Nudge {
        pc: u32,
        surface: u32,
        #[arg(value_enum)]
        direction: NudgeDirection,
        #[arg(long)]
        fine: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum DetachCommand {
    Begin {
        id: u64,
        address: String,
        #[arg(allow_hyphen_values = true)]
        x: i32,
        #[arg(allow_hyphen_values = true)]
        y: i32,
    },
    Release {
        id: u64,
        #[arg(allow_hyphen_values = true)]
        x: i32,
        #[arg(allow_hyphen_values = true)]
        y: i32,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum ShareTarget {
    Blank,
    Window { address: String },
    Workspace { id: i32 },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum FitMode {
    Window,
    Fixed,
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum NudgeDirection {
    Left,
    Right,
    Up,
    Down,
    Nearer,
    Farther,
    Larger,
    Smaller,
}

impl From<NudgeDirection> for Direction {
    fn from(value: NudgeDirection) -> Self {
        match value {
            NudgeDirection::Left => Self::Left,
            NudgeDirection::Right => Self::Right,
            NudgeDirection::Up => Self::Up,
            NudgeDirection::Down => Self::Down,
            NudgeDirection::Nearer => Self::Nearer,
            NudgeDirection::Farther => Self::Farther,
            NudgeDirection::Larger => Self::Larger,
            NudgeDirection::Smaller => Self::Smaller,
        }
    }
}

fn parse_size(raw: &str) -> Result<[u32; 2]> {
    let (width, height) = raw
        .split_once(['x', 'X', '×'])
        .context("Enter WIDTHxHEIGHT.")?;
    let size = [width.trim().parse()?, height.trim().parse()?];
    ensure!(valid_size(size), "{SIZE_REQUIREMENTS}");
    Ok(size)
}

#[derive(serde::Deserialize)]
#[serde(default)]
struct Settings {
    drag_gain: f64,
    default_resolution: [u32; 2],
    resolutions: Vec<[u32; 2]>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            drag_gain: DEFAULT_DRAG_GAIN,
            default_resolution: DEFAULT_SIZE,
            resolutions: vec![[2560, 1440], [1280, 1440]],
        }
    }
}

impl Settings {
    fn load() -> Result<Self> {
        let dir = env::var_os("XDG_CONFIG_HOME")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));
        let Some(path) = dir.map(|dir| dir.join("frame-shell/virtual-panels.json")) else {
            return Ok(Self::default());
        };
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("Read {}", path.display())),
        };
        let settings: Self =
            serde_json::from_slice(&data).with_context(|| format!("Read {}", path.display()))?;
        ensure!(
            valid_size(settings.default_resolution)
                && settings.resolutions.iter().copied().all(valid_size),
            "{}: {SIZE_REQUIREMENTS}",
            path.display()
        );
        Ok(settings)
    }
}

fn control(source: u32, surface: u32, action: Action) -> Request {
    Request::Control {
        source,
        surface,
        action,
    }
}

async fn send(state: &mut WayVRClientState, request: Request) -> Result<Response> {
    smol::future::race(
        WayVRClient::virtual_panel(
            state.wayvr_client.clone(),
            state.serial_generator.increment_get(),
            request,
        ),
        async {
            smol::Timer::after(Duration::from_secs(3)).await;
            bail!(
                "No virtual panel response. Check the headset connection and Frame Shell version."
            );
        },
    )
    .await
}

async fn hosts(state: &mut WayVRClientState) -> Result<Vec<HostPanels>> {
    match send(state, Request::List).await? {
        Response::Hosts(hosts) => Ok(hosts),
        Response::Accepted => bail!("Unexpected response to virtual panel list."),
    }
}

pub async fn run(state: &mut WayVRClientState, command: CommandLine) -> Result<()> {
    let request = match command {
        CommandLine::List => {
            let hosts = hosts(state).await?;
            let json = if state.pretty_print {
                serde_json::to_string_pretty(&hosts)?
            } else {
                serde_json::to_string(&hosts)?
            };
            println!("{json}");
            return Ok(());
        }
        CommandLine::Pick => {
            ensure!(io::stdin().is_terminal(), "Open the picker in a terminal.");
            let hosts = hosts(state).await?;
            let settings = Settings::load()?;
            let gain = settings.drag_gain;
            match smol::unblock(move || pick(hosts, &settings)).await? {
                Some(Pick::Request(request)) => request,
                Some(Pick::Drag(target)) => return drag(state, Some(target), gain).await,
                None => return Ok(()),
            }
        }
        CommandLine::Drag { pc, surface, gain } => {
            let gain = match gain {
                Some(gain) => gain,
                None => Settings::load()?.drag_gain,
            };
            return drag(state, pc.zip(surface), gain).await;
        }
        CommandLine::Refresh { pc } => Request::Refresh { source: pc },
        CommandLine::Detach { gesture } => Request::Detach(match gesture {
            DetachCommand::Begin { id, address, x, y } => DetachGesture::Begin { id, address, cursor: [x, y] },
            DetachCommand::Release { id, x, y } => DetachGesture::Release { id, cursor: [x, y] },
        }),
        CommandLine::Add { pc, size, target } => Request::Create {
            source: pc,
            size: match size {
                Some(size) => size,
                None => Settings::load()?.default_resolution,
            },
            target: match target {
                ShareTarget::Blank => Target::Blank,
                ShareTarget::Window { address } => Target::Window(address),
                ShareTarget::Workspace { id } => Target::Workspace(id),
            },
        },
        CommandLine::Show { pc, surface } => control(pc, surface, Action::Show),
        CommandLine::Hide { pc, surface } => control(pc, surface, Action::Hide),
        CommandLine::Focus { pc, surface } => control(pc, surface, Action::Focus),
        CommandLine::Reset { pc, surface } => control(pc, surface, Action::Reset),
        CommandLine::Close { pc, surface } => control(pc, surface, Action::Close),
        CommandLine::Resize { pc, surface, size } => {
            control(pc, surface, Action::Sizing(Sizing::Resolution(size)))
        }
        CommandLine::Fit { pc, surface, mode } => control(
            pc,
            surface,
            Action::Sizing(Sizing::FitToWindow(matches!(mode, FitMode::Window))),
        ),
        CommandLine::Nudge {
            pc,
            surface,
            direction,
            fine,
        } => control(
            pc,
            surface,
            Action::Nudge {
                direction: direction.into(),
                fine,
            },
        ),
    };
    accept(state, request).await
}

async fn accept(state: &mut WayVRClientState, request: Request) -> Result<()> {
    match send(state, request).await? {
        Response::Accepted => Ok(()),
        Response::Hosts(_) => bail!("Unexpected response to virtual panel command."),
    }
}

#[derive(serde::Deserialize)]
struct Cursor {
    x: f64,
    y: f64,
}

#[derive(serde::Deserialize)]
struct Monitor {
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    transform: u32,
}

fn hyprland_query<T: serde::de::DeserializeOwned>(query: &str) -> Result<T> {
    let result = Command::new("hyprctl")
        .args(["-j", query])
        .output()
        .context("Run hyprctl in the PC's Hyprland session.")?;
    ensure!(result.status.success(), "Hyprland query {query} failed.");
    serde_json::from_slice(&result.stdout).with_context(|| format!("Read Hyprland {query}"))
}

fn output_under_cursor() -> Result<String> {
    let cursor: Cursor = hyprland_query("cursorpos")?;
    let monitors: Vec<Monitor> = hyprland_query("monitors")?;
    monitors
        .into_iter()
        .find(|m| {
            let (w, h) = if m.transform % 2 == 0 {
                (m.width, m.height)
            } else {
                (m.height, m.width)
            };
            cursor.x >= m.x
                && cursor.x < m.x + w / m.scale
                && cursor.y >= m.y
                && cursor.y < m.y + h / m.scale
        })
        .map(|m| m.name)
        .context("No Hyprland output under the cursor.")
}

async fn drag(state: &mut WayVRClientState, explicit: Option<(u32, u32)>, gain: f64) -> Result<()> {
    ensure!(
        (0.0001..=0.01).contains(&gain),
        "Drag gain must be between 0.0001 and 0.01."
    );
    let output = smol::unblock(output_under_cursor).await?;
    let begin = match explicit {
        Some((source, surface)) => Request::BeginDrag { source, surface },
        None => Request::BeginDragAtOutput {
            output: output.clone(),
        },
    };
    accept(state, begin).await?;

    let (tx, rx) = smol::channel::bounded(4);
    let capture = smol::spawn(smol::unblock(move || capture_drag::run(&output, gain, &tx)));
    let mut motion_error = None;
    while let Ok(request) = rx.recv().await {
        if let Err(err) = accept(state, request).await {
            motion_error = Some(err);
            rx.close();
            break;
        }
    }
    let capture = capture.await;
    let commit = motion_error.is_none() && capture.as_ref().is_ok_and(|commit| *commit);
    let finish = accept(state, Request::EndDrag { commit }).await;
    if let Some(err) = motion_error {
        return Err(err);
    }
    capture?;
    finish
}

enum Choice {
    Existing(wayvr_ipc::virtual_panel::Panel),
    Share {
        target: Target,
        already_shared: bool,
    },
}

enum Pick {
    Request(Request),
    Drag((u32, u32)),
}

fn pick(hosts: Vec<HostPanels>, settings: &Settings) -> Result<Option<Pick>> {
    let labels: Vec<_> = hosts.iter().map(|host| host.name.clone()).collect();
    let index = match labels.len() {
        0 => bail!("No connected PCs support virtual panels."),
        1 => 0,
        _ => match choose("PC", &labels)? {
            Some(index) => index,
            None => return Ok(None),
        },
    };
    let host = hosts
        .into_iter()
        .nth(index)
        .context("Invalid PC selection.")?;
    let mut entries = Vec::new();
    let can_add = host.panels.len() < host.max_panels as usize;
    for panel in host.panels {
        let shown = if panel.visible { "shown" } else { "hidden" };
        let detail = panel
            .info
            .fit_status
            .as_deref()
            .unwrap_or(match panel.info.fit_to_window {
                Some(true) => "Fit to window",
                _ => "Fixed resolution",
            });
        entries.push((
            format!(
                "{} · {} · {}×{} ({shown}) · {detail}",
                panel.info.output_name, panel.info.title, panel.info.width, panel.info.height
            ),
            Choice::Existing(panel),
        ));
    }
    if can_add {
        entries.push((
            "New blank output".into(),
            Choice::Share {
                target: Target::Blank,
                already_shared: false,
            },
        ));
    }
    for window in host
        .targets
        .windows
        .into_iter()
        .filter(|w| can_add || w.already_shared)
    {
        let label = if window.title.is_empty() {
            window.app_id
        } else {
            window.title
        };
        let kind = if window.already_shared {
            "Show window"
        } else {
            "Window"
        };
        entries.push((
            format!("{kind} · {label}"),
            Choice::Share {
                target: Target::Window(window.address),
                already_shared: window.already_shared,
            },
        ));
    }
    for workspace in host
        .targets
        .workspaces
        .into_iter()
        .filter(|w| can_add || w.already_shared)
    {
        let kind = if workspace.already_shared {
            "Show workspace"
        } else {
            "Workspace"
        };
        entries.push((
            format!("{kind} · {}", workspace.name),
            Choice::Share {
                target: Target::Workspace(workspace.id),
                already_shared: workspace.already_shared,
            },
        ));
    }

    let labels: Vec<_> = entries.iter().map(|(label, _)| label.clone()).collect();
    let Some(index) = choose("Virtual panel", &labels)? else {
        return Ok(None);
    };
    let (_, choice) = entries.swap_remove(index);
    match choice {
        Choice::Share {
            target,
            already_shared,
        } => {
            let size = if already_shared {
                Some(settings.default_resolution)
            } else {
                pick_size(settings)?
            };
            Ok(size.map(|size| {
                Pick::Request(Request::Create {
                    source: host.source,
                    target,
                    size,
                })
            }))
        }
        Choice::Existing(panel) => pick_action(
            host.source,
            &panel,
            host.resize_panel,
            host.drag_panel,
            settings,
        ),
    }
}

fn pick_size(settings: &Settings) -> Result<Option<[u32; 2]>> {
    let mut sizes = vec![settings.default_resolution];
    for &size in &settings.resolutions {
        if !sizes.contains(&size) {
            sizes.push(size);
        }
    }
    let mut labels: Vec<_> = sizes.iter().map(|[w, h]| format!("{w}×{h}")).collect();
    labels.push("Custom resolution…".into());
    match choose("Resolution", &labels)? {
        Some(index) if index == sizes.len() => {
            eprint!("Resolution (WIDTHxHEIGHT, Enter to cancel): ");
            io::stderr().flush()?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            let size = line.trim();
            if size.is_empty() {
                return Ok(None);
            }
            Ok(Some(parse_size(size)?))
        }
        Some(index) => Ok(sizes.get(index).copied()),
        None => Ok(None),
    }
}

fn pick_action(
    source: u32,
    panel: &wayvr_ipc::virtual_panel::Panel,
    can_resize: bool,
    can_drag: bool,
    settings: &Settings,
) -> Result<Option<Pick>> {
    let visible = panel.visible;
    let mut entries = vec![
        ("Focus", Action::Focus),
        if visible {
            ("Hide", Action::Hide)
        } else {
            ("Show", Action::Show)
        },
        ("Reset position", Action::Reset),
    ];
    if visible {
        for (label, direction) in [
            ("Move left", Direction::Left),
            ("Move right", Direction::Right),
            ("Move up", Direction::Up),
            ("Move down", Direction::Down),
            ("Move nearer", Direction::Nearer),
            ("Move farther", Direction::Farther),
            ("Make larger", Direction::Larger),
            ("Make smaller", Direction::Smaller),
        ] {
            entries.push((
                label,
                Action::Nudge {
                    direction,
                    fine: false,
                },
            ));
        }
    }
    entries.push(("Close", Action::Close));
    if can_resize && let Some(fit) = panel.info.fit_to_window {
        let label = if fit {
            "Fixed resolution"
        } else {
            "Fit to window"
        };
        entries.push((label, Action::Sizing(Sizing::FitToWindow(!fit))));
    }
    let mut labels: Vec<_> = entries.iter().map(|(label, _)| *label).collect();
    let resize = can_resize && visible;
    let drag = can_drag && visible;
    if resize {
        labels.push("Display resolution…");
    }
    if drag {
        labels.push("Drag with mouse");
    }
    let Some(index) = choose("Action", &labels)? else {
        return Ok(None);
    };
    let surface = panel.info.surface;
    if let Some((_, action)) = entries.get(index) {
        return Ok(Some(Pick::Request(control(source, surface, *action))));
    }
    if resize && index == entries.len() {
        return Ok(pick_size(settings)?.map(|size| {
            Pick::Request(control(
                source,
                surface,
                Action::Sizing(Sizing::Resolution(size)),
            ))
        }));
    }
    Ok(Some(Pick::Drag((source, surface))))
}

/// fzf handles terminal modes, arrow keys and Escape. Numeric row IDs are
/// generated locally, and labels from the PC are displayed as plain text.
fn choose(prompt: &str, labels: &[impl AsRef<str>]) -> Result<Option<usize>> {
    if labels.is_empty() {
        return Ok(None);
    }
    let mut child = Command::new("fzf")
        .args([
            "--delimiter",
            "\t",
            "--with-nth",
            "2..",
            "--prompt",
            &format!("{prompt}> "),
        ])
        .env_remove("FZF_DEFAULT_OPTS")
        .env_remove("FZF_DEFAULT_OPTS_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("The terminal picker requires fzf.")?;
    let written = (|| -> Result<()> {
        let mut input = child.stdin.take().context("Missing picker input.")?;
        for (index, label) in labels.iter().enumerate() {
            let label: String = label
                .as_ref()
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            writeln!(input, "{index}\t{label}")?;
        }
        Ok(())
    })();
    let output = child.wait_with_output()?;
    if matches!(output.status.code(), Some(1 | 130)) {
        return Ok(None);
    }
    ensure!(
        output.status.success(),
        "Terminal picker exited with {}.",
        output.status
    );
    written?;
    let row = std::str::from_utf8(&output.stdout)?;
    let (index, _) = row.split_once('\t').context("Invalid picker selection.")?;
    let index: usize = index.parse()?;
    ensure!(index < labels.len(), "Invalid picker row.");
    Ok(Some(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn add_parses_custom_resolution() {
        let args = crate::Args::try_parse_from([
            "wayvrctl",
            "virtual-panel",
            "add",
            "1",
            "--size",
            "1600x1000",
            "blank",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            crate::Subcommands::VirtualPanel {
                command: CommandLine::Add {
                    pc: 1,
                    size: Some([1600, 1000]),
                    target: ShareTarget::Blank
                },
            }
        ));
    }

    #[test]
    fn rejects_dimensions_that_would_be_rounded_or_clamped() {
        for size in [
            "641x480",
            "640x481",
            "4000x1080",
            "1920x2162",
            "1080",
            "0x0",
        ] {
            assert!(parse_size(size).is_err(), "{size}");
        }
        assert_eq!(parse_size("1280×1440").unwrap(), [1280, 1440]);
    }
}
