use crate::{
	assets::AssetPathRef,
	components::{ComponentTrait, button::ComponentButton},
	drawing::Boundary,
	globals::WguiGlobals,
	i18n::Translation,
	layout::{Layout, WidgetID},
	parser::{self, Fetchable, ParserState, TemplateParams},
	task::Tasks,
	windowing::window::{WguiWindow, WguiWindowParams, WguiWindowParamsExtra, WguiWindowPlacement},
};
use glam::Vec2;
use std::rc::Rc;

#[derive(Default, Clone)]
pub struct Button {
	pub title: Translation,
	pub tooltip: Option<Translation>,
	pub action_name: Option<Rc<str>>,
	pub sprite_src_builtin: Option<Rc<str>>,
	pub attribs: Vec<parser::AttribPair>,
}

#[derive(Clone)]
pub enum Cell {
	Button(Button),
	Separator,
}

pub enum Blueprint {
	Cells(Vec<Cell>),
	Template {
		template_name: Rc<str>,
		template_params: TemplateParams,
	},
}

#[derive(Clone)]
pub enum Position {
	Unspecified,
	Absolute(Vec2),
	Widget(WidgetID),
	Boundary(Boundary),
}

// absolute position
impl From<Vec2> for Position {
	fn from(value: Vec2) -> Self {
		Self::Absolute(value)
	}
}

impl From<Option<Vec2>> for Position {
	fn from(value: Option<Vec2>) -> Self {
		match value {
			Some(v) => Self::Absolute(v),
			None => Self::Unspecified,
		}
	}
}

// relative to widget center
impl From<WidgetID> for Position {
	fn from(value: WidgetID) -> Self {
		Self::Widget(value)
	}
}

// relative to absolute rectangle center
impl From<Boundary> for Position {
	fn from(value: Boundary) -> Self {
		Self::Boundary(value)
	}
}

pub struct OpenParams {
	pub on_custom_attribs: Option<parser::OnCustomAttribsFunc>,
	pub position: Position,
	pub blueprint: Blueprint,
}

#[derive(Clone)]
enum Task {
	ActionClicked(Option<Rc<str>>),
}

#[derive(Default)]
pub struct ContextMenu {
	window: WguiWindow,
	pending_open: Option<OpenParams>,
	tasks: Tasks<Task>,
}

fn doc_params<'a>(globals: &WguiGlobals, max_height: f32) -> parser::ParseDocumentParams<'a> {
	let mut extra = parser::ParseDocumentExtra::default();
	extra
		.extra_vars
		.insert("max_height".into(), format!("{max_height:.0}").into());
	parser::ParseDocumentParams {
		globals: globals.clone(),
		path: AssetPathRef::WguiInternal("wgui/context_menu.xml"),
		extra,
	}
}

#[derive(Default)]
pub enum TickResult {
	/// Nothing happened
	#[default]
	None,
	/// The context menu was opened.
	Opened,
	/// User has selected an action.
	Action(Rc<str>),
	/// The context menu was closed without an action.
	Closed,
}

impl ContextMenu {
	pub fn open(&mut self, params: OpenParams) {
		self.pending_open = Some(params);
	}

	pub fn close(&self) {
		self.window.close();
	}

	fn process_open(
		&mut self,
		params: OpenParams,
		layout: &mut Layout,
		parser_state: &mut ParserState,
	) -> anyhow::Result<()> {
		let cells = match params.blueprint {
			Blueprint::Template {
				template_name,
				template_params,
			} => parser_state.context_menu_parse_cells(&template_name, &template_params)?,
			Blueprint::Cells(cells) => cells,
		};

		let absolute_pos = match params.position {
			Position::Unspecified => Vec2::new(0.0, 0.0),
			Position::Absolute(pos) => pos,
			Position::Boundary(boundary) => boundary.center(),
			Position::Widget(widget_id) => {
				let widget = layout.state.widgets.fetch(widget_id)?;
				widget.state().data.cached_absolute_boundary.center()
			}
		};

		let root_size = layout.state.get_widget_size(layout.content_root_widget);
		let avail_h = if root_size.y > 0.0 {
			root_size.y
		} else {
			layout.content_size.y
		};
		let max_height = (avail_h - 40.0).max(96.0);

		let (window_pos, placement) = if absolute_pos.y < root_size.y / 2.0 {
			(absolute_pos, WguiWindowPlacement::TopLeft)
		} else {
			// invert y axis (position.y is counted from the bottom)
			(
				Vec2::new(absolute_pos.x, root_size.y - absolute_pos.y),
				WguiWindowPlacement::BottomLeft,
			)
		};

		let globals = layout.state.globals.clone();

		self.window.open(&mut WguiWindowParams {
			layout,
			position: window_pos,
			extra: WguiWindowParamsExtra {
				with_decorations: false,
				close_if_clicked_outside: true,
				placement,
				..Default::default()
			},
		})?;

		let content = self.window.get_content();
		let doc_params = doc_params(&globals, max_height);

		let mut inner_parser = parser::parse_from_assets(&doc_params, layout, content.id)?;

		let id_buttons = inner_parser.get_widget_id("buttons")?;

		let mut has_sprites = false;

		for cell in &cells {
			if let Cell::Button(button) = cell
				&& button.sprite_src_builtin.is_some()
			{
				has_sprites = true;
			}
		}

		for cell in &cells {
			match cell {
				Cell::Button(cell) => {
					let mut par = TemplateParams::new();
					par.insert_rc("text", cell.title.generate(&mut globals.i18n()));

					if has_sprites {
						par.insert_rc(
							"sprite_src_builtin",
							cell
								.sprite_src_builtin
								.clone()
								.unwrap_or_else(|| Rc::from("@/dashboard/dot.svg")),
						);
					}
					if let Some(tooltip) = &cell.tooltip {
						par.insert_rc("tooltip_str", tooltip.generate(&mut globals.i18n()));
					}

					let mut data_cell = inner_parser.parse_template_only(
						&doc_params,
						if has_sprites { "CellButtonSprite" } else { "CellButton" },
						layout,
						id_buttons,
						par,
					)?;

					let button = data_cell.fetch_component_as::<ComponentButton>("button")?;
					let button_id = button.base().get_id();
					parser_state.data.take_results_from(&mut data_cell);
					self
						.tasks
						.handle_button(&button, Task::ActionClicked(cell.action_name.clone()));

					if let Some(c) = params.on_custom_attribs.as_ref() {
						(*c)(parser::CustomAttribsInfo {
							pairs: &cell.attribs,
							parent_id: id_buttons,
							widget_id: button_id,
							widgets: &layout.state.widgets,
						});
					}
				}
				Cell::Separator => {
					inner_parser.realize_template(&doc_params, "Separator", layout, id_buttons, Default::default())?;
				}
			}
		}
		Ok(())
	}

	pub fn tick(&mut self, layout: &mut Layout, parser_state: &mut ParserState) -> anyhow::Result<TickResult> {
		if let Some(p) = self.pending_open.take() {
			self.process_open(p, layout, parser_state)?;
			self.window.reposition_if_outside(layout);
			let _ = self.tasks.drain();
			return Ok(TickResult::Opened);
		}

		let mut result = TickResult::default();

		for task in self.tasks.drain() {
			match task {
				Task::ActionClicked(Some(action_name)) => {
					result = TickResult::Action(action_name);
					self.close();
				}
				Task::ActionClicked(None) => {
					result = TickResult::Closed;
					self.close();
				}
			}
		}

		Ok(result)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		assets::{AssetProvider, LangProvider},
		event::{DeviceBitmask, Event, MouseMotionEvent, MouseWheelEvent},
		font_config::WguiFontConfig,
		i18n::LangsList,
		layout::{Layout, LayoutParams, LayoutUpdateParams, WidgetID},
		palette::WguiColorPalette,
		widget::EventResult,
	};
	use std::path::PathBuf;

	struct DummyLangs;
	impl LangsList for DummyLangs {
		fn all_locale(&self) -> &'static [&'static str] {
			&["en"]
		}
		fn default_lang(&self) -> &'static str {
			"en"
		}
	}

	struct DummyLangProvider(DummyLangs);
	impl LangProvider for DummyLangProvider {
		fn langs_list(&self) -> &dyn LangsList {
			&self.0
		}
		fn forced_lang(&self) -> Option<&str> {
			Some("en")
		}
	}

	struct DummyAssets;
	impl AssetProvider for DummyAssets {
		fn load_from_path(&mut self, path: &str) -> anyhow::Result<Vec<u8>> {
			if path.ends_with(".json") {
				Ok(b"{}".to_vec())
			} else if path == "dummy.xml" {
				Ok(
					b"<layout version=\"2\"><elements><div width=\"100%\" height=\"100%\"></div></elements></layout>"
						.to_vec(),
				)
			} else {
				anyhow::bail!("missing test asset: {path}")
			}
		}
	}

	fn make_layout(width: f32, height: f32) -> (Layout, ParserState) {
		let globals = WguiGlobals::new(
			Box::new(DummyAssets),
			&DummyLangProvider(DummyLangs),
			&WguiFontConfig {
				binaries: Vec::new(),
				family_name_sans_serif: "sans-serif",
				family_name_serif: "serif",
				family_name_monospace: "monospace",
			},
			PathBuf::from("/tmp"),
			WguiColorPalette::get_builtin("default"),
		)
		.expect("create WguiGlobals");
		let (mut layout, parser_state) = parser::new_layout_from_assets(
			&parser::ParseDocumentParams {
				globals,
				path: AssetPathRef::BuiltIn("dummy.xml"),
				extra: Default::default(),
			},
			LayoutParams {
				resize_to_parent: true,
				..Default::default()
			},
		)
		.expect("create Layout");
		layout
			.update(&mut LayoutUpdateParams {
				size: Vec2::new(width, height),
				timestep_alpha: 1.0,
			})
			.expect("initial layout update");
		(layout, parser_state)
	}

	fn find_scrollable_widget(layout: &Layout, root: WidgetID) -> WidgetID {
		let mut ids = Vec::new();
		layout.collect_children_ids_recursive(root, &mut ids);
		ids.into_iter()
			.find(|&id| {
				layout
					.state
					.get_widget_style(id)
					.is_some_and(|s| s.overflow.y == taffy::Overflow::Scroll)
			})
			.expect("scrollable context menu container")
	}

	#[test]
	fn overflowing_context_menu_clamps_and_scrolls() {
		let (mut layout, mut parser_state) = make_layout(800.0, 400.0);
		let mut menu = ContextMenu::default();

		let mut cells = Vec::new();
		for i in 0..25 {
			if i == 5 || i == 15 {
				cells.push(Cell::Separator);
			}
			cells.push(Cell::Button(Button {
				title: Translation::from_raw_text_string(format!("Item {i}")),
				..Default::default()
			}));
		}

		menu.open(OpenParams {
			on_custom_attribs: None,
			position: Position::Absolute(Vec2::new(40.0, 360.0)),
			blueprint: Blueprint::Cells(cells),
		});
		assert!(matches!(
			menu.tick(&mut layout, &mut parser_state).expect("tick menu"),
			TickResult::Opened
		));
		layout.process_tasks().expect("apply reposition padding task");
		layout
			.update(&mut LayoutUpdateParams {
				size: Vec2::new(800.0, 400.0),
				timestep_alpha: 1.0,
			})
			.expect("recompute clamped menu layout");

		let content_id = menu.window.get_content().id;
		let content_bounds = layout.state.get_widget_boundary(content_id);
		assert!(
			content_bounds.top() >= -0.5 && content_bounds.bottom() <= 400.5,
			"menu bounds outside overlay: top={} bottom={}",
			content_bounds.top(),
			content_bounds.bottom()
		);

		let scroll_id = find_scrollable_widget(&layout, content_id);
		let scroll_node = *layout.state.nodes.get(scroll_id).unwrap();
		let scroll_layout = layout.state.tree.layout(scroll_node).unwrap();
		assert!(
			scroll_layout.scroll_height() > 100.0,
			"expected vertical overflow > 100, got {}",
			scroll_layout.scroll_height()
		);

		let first_button_id = parser_state.data.components.first().unwrap().0.base().get_id();
		let first_button_size = layout.state.get_widget_size(first_button_id);
		assert!(
			first_button_size.y >= 28.0,
			"expected cell height >= 28 without flex shrinking, got {}",
			first_button_size.y
		);

		layout
			.push_event(
				&Event::MouseMotion(MouseMotionEvent {
					pos: Vec2::ZERO,
					device: DeviceBitmask::from_index(0),
				}),
				&mut (),
				&mut (),
			)
			.expect("populate absolute widget bounds");

		let (scroll_bounds, scroll_before) = {
			let scroll_layout = layout.state.tree.layout(scroll_node).unwrap();
			let widget = layout.state.widgets.get(scroll_id).unwrap();
			let st = widget.state();
			let info = crate::widget::get_scrollbar_info(scroll_layout).unwrap();
			(
				st.data.cached_absolute_boundary,
				st.get_scroll_shift_raw(&info, scroll_layout),
			)
		};
		let wheel_res = layout
			.push_event(
				&Event::MouseWheel(MouseWheelEvent {
					pos: scroll_bounds.center(),
					delta: Vec2::new(0.0, -1.0),
					device: DeviceBitmask::from_index(0),
				}),
				&mut (),
				&mut (),
			)
			.expect("push wheel event");
		assert!(matches!(wheel_res, EventResult::Consumed));

		let scroll_after = {
			let scroll_layout = layout.state.tree.layout(scroll_node).unwrap();
			let widget = layout.state.widgets.get(scroll_id).unwrap();
			let info = crate::widget::get_scrollbar_info(scroll_layout).unwrap();
			widget.state().get_scroll_shift_raw(&info, scroll_layout)
		};
		assert!(
			scroll_after.y > scroll_before.y,
			"expected wheel down to advance scroll shift: before={scroll_before:?} after={scroll_after:?}"
		);
	}

	#[test]
	fn short_context_menu_does_not_overflow() {
		let (mut layout, mut parser_state) = make_layout(800.0, 400.0);
		let mut menu = ContextMenu::default();

		let cells = (0..3)
			.map(|i| {
				Cell::Button(Button {
					title: Translation::from_raw_text_string(format!("Short {i}")),
					..Default::default()
				})
			})
			.collect();

		menu.open(OpenParams {
			on_custom_attribs: None,
			position: Position::Absolute(Vec2::new(40.0, 200.0)),
			blueprint: Blueprint::Cells(cells),
		});
		assert!(matches!(
			menu.tick(&mut layout, &mut parser_state).expect("tick menu"),
			TickResult::Opened
		));
		layout.process_tasks().expect("apply reposition padding task");
		layout
			.update(&mut LayoutUpdateParams {
				size: Vec2::new(800.0, 400.0),
				timestep_alpha: 1.0,
			})
			.expect("recompute short menu layout");

		let content_id = menu.window.get_content().id;
		let scroll_id = find_scrollable_widget(&layout, content_id);
		let scroll_node = *layout.state.nodes.get(scroll_id).unwrap();
		let scroll_layout = layout.state.tree.layout(scroll_node).unwrap();
		assert!(
			scroll_layout.scroll_height() < 3.0,
			"short menu should not overflow, got scroll_height={}",
			scroll_layout.scroll_height()
		);
	}
}
