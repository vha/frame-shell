//! Keyboard-driven controls for Frame Shell virtual PC panels.

use serde::{Deserialize, Serialize};

use crate::frame::{VirtualPanelInfo, VirtualTargets};

pub const DEFAULT_SIZE: [u32; 2] = [1920, 1080];

pub const MIN_SIZE: [u32; 2] = [640, 480];
pub const MAX_SIZE: [u32; 2] = [3840, 2160];

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
	BeginDrag {
		source: u32,
		surface: u32,
	},
	BeginDragAtOutput {
		output: String,
	},
	DragMotion {
		translation: [f32; 3],
		scale_steps: f32,
	},
	EndDrag {
		commit: bool,
	},
	Detach(DetachGesture),
	Control {
		source: u32,
		surface: u32,
		action: Action,
	},
	DragResize {
		delta: [f32; 2],
	},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DetachGesture {
	Begin {
		id: u64,
		address: String,
		cursor: [i32; 2],
	},
	Release {
		id: u64,
		cursor: [i32; 2],
	},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DropEdge {
	Left,
	Right,
	Top,
	Bottom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetachPlacement {
	pub anchor: String,
	pub anchor_size: [i32; 2],
	pub edge: DropEdge,
	pub offset: i32,
	pub size: [u32; 2],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
	Blank,
	Window(String),
	Workspace(i32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Sizing {
	Resolution([u32; 2]),
	FitToWindow(bool),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Action {
	Show,
	Hide,
	Focus,
	Reset,
	Close,
	Nudge { direction: Direction, fine: bool },
	Sizing(Sizing),
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
	#[serde(default)]
	pub resize_panel: bool,
	#[serde(default)]
	pub drag_panel: bool,
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
