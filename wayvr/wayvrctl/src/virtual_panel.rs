use std::{
    io::{self, IsTerminal, Write},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use wayvr_ipc::{
    client::WayVRClient,
    virtual_panel::{
        Action, DEFAULT_SIZE, Direction, HostPanels, Request, Response, SIZE_REQUIREMENTS, Target,
        valid_size,
    },
};

use crate::helper::WayVRClientState;

#[derive(Debug, clap::Subcommand)]
pub enum CommandLine {
    /// List connected PCs, active panels and available share targets as JSON
    List,
    /// Refresh windows and workspaces on a PC
    Refresh { pc: u32 },
    /// Choose a PC, panel or share target in an interactive terminal picker
    Pick,
    /// Share a blank screen, window or workspace at the given resolution
    Add {
        pc: u32,
        #[arg(long, default_value = "1920x1080", value_parser = parse_size)]
        size: [u32; 2],
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
pub enum ShareTarget {
    Blank,
    Window { address: String },
    Workspace { id: i32 },
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
            let Some(request) = smol::unblock(move || pick(hosts)).await? else {
                return Ok(());
            };
            request
        }
        CommandLine::Refresh { pc } => Request::Refresh { source: pc },
        CommandLine::Add { pc, size, target } => Request::Create {
            source: pc,
            size,
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
    match send(state, request).await? {
        Response::Accepted => Ok(()),
        Response::Hosts(_) => bail!("Unexpected response to virtual panel command."),
    }
}

enum Choice {
    Existing(u32, bool),
    Share {
        target: Target,
        already_shared: bool,
    },
}

fn pick(hosts: Vec<HostPanels>) -> Result<Option<Request>> {
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
    for panel in &host.panels {
        let shown = if panel.visible { "shown" } else { "hidden" };
        entries.push((
            format!(
                "{} · {} ({shown})",
                panel.info.output_name, panel.info.title
            ),
            Choice::Existing(panel.info.surface, panel.visible),
        ));
    }
    let can_add = host.panels.len() < host.max_panels as usize;
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
                Some(DEFAULT_SIZE)
            } else {
                pick_size()?
            };
            Ok(size.map(|size| Request::Create {
                source: host.source,
                target,
                size,
            }))
        }
        Choice::Existing(surface, visible) => {
            Ok(pick_action(visible)?.map(|action| control(host.source, surface, action)))
        }
    }
}

fn pick_size() -> Result<Option<[u32; 2]>> {
    let labels = [
        "1920×1080",
        "2560×1440",
        "1280×1440 portrait",
        "Custom resolution…",
    ];
    match choose("Resolution", &labels)? {
        Some(0) => Ok(Some(DEFAULT_SIZE)),
        Some(1) => Ok(Some([2560, 1440])),
        Some(2) => Ok(Some([1280, 1440])),
        Some(3) => {
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
        _ => Ok(None),
    }
}

fn pick_action(visible: bool) -> Result<Option<Action>> {
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
    let labels: Vec<_> = entries.iter().map(|(label, _)| *label).collect();
    Ok(choose("Action", &labels)?.map(|index| entries[index].1))
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
                    size: [1600, 1000],
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
