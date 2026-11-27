//! Keyboard-driven controls for Frame Shell virtual PC panels.

use serde::{Deserialize, Serialize};

use crate::frame::{VirtualPanelInfo, VirtualTargets};

pub const DEFAULT_SIZE: [u32; 2] = [1920, 1080];

const MIN_SIZE: [u32; 2] = [640, 480];
const MAX_SIZE: [u32; 2] = [3840, 2160];

/// Preserve the host's default and clamping behavior for existing callers.
pub fn normalized_size(size: [u32; 2]) -> [u32; 2] {
	std::array::from_fn(|axis| {
		let value = if size[axis] == 0 {
			DEFAULT_SIZE[axis]
		} else {
			size[axis]
		};
		value.clamp(MIN_SIZE[axis], MAX_SIZE[axis]) & !1
	})
}

pub fn valid_size(size: [u32; 2]) -> bool {
	normalized_size(size) == size
}

pub const SIZE_REQUIREMENTS: &str = "Use even dimensions, 640–3840 wide and 480–2160 high.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
	List,
	Refresh {
		source: u32,
	},
	Create {
		source: u32,
		target: Target,
		size: [u32; 2],
	},
	Control {
		source: u32,
		surface: u32,
		action: Action,
	},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
	Blank,
	Window(String),
	Workspace(i32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Action {
	Show,
	Hide,
	Focus,
	Reset,
	Close,
	Nudge { direction: Direction, fine: bool },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Direction {
	Left,
	Right,
	Up,
	Down,
	Nearer,
	Farther,
	Larger,
	Smaller,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostPanels {
	pub source: u32,
	pub name: String,
	pub max_panels: u32,
	pub panels: Vec<Panel>,
	pub targets: VirtualTargets,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Panel {
	pub info: VirtualPanelInfo,
	pub visible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
	Hosts(Vec<HostPanels>),
	Accepted,
}
