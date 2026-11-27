use crate::{design, i18n::Language, icons};
use model::voice_overlay::{
	Appearance, FontWeight, LayoutAck, LayoutAction, LayoutCommand, LayoutPanel, LayoutPositions,
	LayoutStatus, NotificationSettings, Position, RosterMode, ToastCorner,
};
use std::{
	sync::Arc,
	time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
	pub monitor: Option<String>,
	pub position: Option<Position>,
	pub appearance: Appearance,
	pub roster: RosterMode,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Display {
	pub id: u32,
	pub name: String,
	pub label: String,
	pub logical_size: (i32, i32),
	pub live_supported: bool,
	pub notifications_supported: bool,
	pub layout_supported: bool,
}

impl Display {
	fn selectable(&self) -> bool {
		self.live_supported || self.notifications_supported
	}

	fn caption(&self) -> String {
		if self.label == self.name {
			self.name.clone()
		} else {
			format!("{} ({})", self.label, self.name)
		}
	}
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Availability {
	#[default]
	Starting,
	Unavailable,
	EditingUnavailable,
	Available,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
	Identify(Instant),
	Preview,
}

pub struct Placement {
	pub generation: u64,
	pub monitor: String,
	pub initial: LayoutPositions,
	pub corner: ToastCorner,
	pub mode: Mode,
	pub command: Option<LayoutCommand>,
}

struct ActivePlacement {
	request: Placement,
	output: (u32, (i32, i32)),
	display_before_preview: Option<String>,
	ack: Option<LayoutAck>,
	sequence: u64,
}

impl ActivePlacement {
	fn movable_ack(&self) -> Option<&LayoutAck> {
		self.ack
			.as_ref()
			.filter(|ack| !ack.dragging && ack.status != LayoutStatus::TooSmall)
	}

	fn commandable_ack(&self) -> Option<&LayoutAck> {
		self.movable_ack()
			.filter(|_| self.request.command.is_none())
	}
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum Persistence {
	#[default]
	Idle,
	Saving,
	NotLoaded,
	Failed {
		retry: bool,
	},
}

#[derive(Default)]
pub struct Settings {
	pub enabled: bool,
	pub saved: Layout,
	pub fonts: Arc<[String]>,
	pub paused_after_move: bool,
	pub persistence: Persistence,
	pub layout_persistence: Persistence,
	availability: Availability,
	displays: Vec<Display>,
	draft: Option<Layout>,
	placement: Option<ActivePlacement>,
	generation: u64,
	layout_interrupted: bool,
	save_requested: bool,
	retry_requested: bool,
	layout_retry_requested: bool,
}

impl Settings {
	pub fn take_retry_request(&mut self) -> bool {
		std::mem::take(&mut self.retry_requested)
	}

	pub fn take_layout_retry_request(&mut self) -> bool {
		std::mem::take(&mut self.layout_retry_requested)
	}

	pub(super) fn paused_after_move(&self) -> bool {
		self.paused_after_move && self.enabled && !self.previewing()
	}

	fn layout(&self) -> &Layout {
		self.draft.as_ref().unwrap_or(&self.saved)
	}

	pub fn set_displays(&mut self, availability: Availability, displays: Vec<Display>) {
		self.availability = availability;
		self.displays = displays;
		if self.placement.as_ref().is_some_and(|placement| {
			let request = &placement.request;
			!self.placement_supported(&request.monitor, request.mode)
				|| self
					.display(&request.monitor)
					.is_none_or(|display| (display.id, display.logical_size) != placement.output)
		}) {
			self.interrupt_placement();
		}
	}

	pub fn placement(&self) -> Option<&Placement> {
		self.placement.as_ref().map(|placement| &placement.request)
	}

	fn display(&self, monitor: &str) -> Option<&Display> {
		self.displays.iter().find(|display| display.name == monitor)
	}

	fn available(&self) -> bool {
		matches!(
			self.availability,
			Availability::Available | Availability::EditingUnavailable
		)
	}

	pub fn live_supported(&self, monitor: &str) -> bool {
		self.available()
			&& self
				.display(monitor)
				.is_some_and(|display| display.live_supported)
	}

	fn display_choice_enabled(&self, display: &Display) -> bool {
		if self.previewing() {
			display.layout_supported
		} else {
			display.selectable()
		}
	}

	fn placement_supported(&self, monitor: &str, mode: Mode) -> bool {
		self.availability == Availability::Available
			&& self.display(monitor).is_some_and(|display| match mode {
				Mode::Identify(_) => display.live_supported,
				Mode::Preview => display.layout_supported,
			})
	}

	fn matches(&self, generation: u64, monitor: &str) -> bool {
		self.placement()
			.is_some_and(|request| request.generation == generation && request.monitor == monitor)
	}

	pub fn report_closed(&mut self, generation: u64, monitor: &str) {
		if self.matches(generation, monitor) {
			self.interrupt_placement();
		}
	}

	pub fn report_layout(&mut self, generation: u64, monitor: &str, ack: LayoutAck) {
		if self.matches(generation, monitor)
			&& let Some(placement) = self.preview_mut()
			&& ack.command <= placement.sequence
			&& placement.ack.as_ref().is_none_or(|previous| {
				ack.command >= previous.command && ack.revision >= previous.revision
			}) {
			if placement
				.request
				.command
				.as_ref()
				.is_some_and(|pending| pending.sequence == ack.command)
			{
				placement.request.command = None;
			}
			placement.ack = Some(ack);
		}
	}

	/// Apply an Approved event, not an ordinary acknowledgement of a layout command.
	pub fn apply_layout(
		&mut self,
		generation: u64,
		monitor: &str,
		ack: LayoutAck,
		notifications: &mut NotificationSettings,
	) -> bool {
		if self.matches(generation, monitor)
			&& let Some(placement) = self.preview()
			&& let Some(command) = &placement.request.command
			&& let LayoutAction::Save { revision } = command.action
			&& command.sequence == ack.command
			&& ack.revision == revision
			&& ack.status == LayoutStatus::Ready
			&& !ack.dragging
			&& placement.ack.as_ref().is_some_and(|latest| {
				latest.revision == revision
					&& latest.status == LayoutStatus::Ready
					&& !latest.dragging
					&& latest.positions == ack.positions
			}) && let Some(mut draft) = self.draft.take()
		{
			draft.position = ack.positions.voice;
			self.saved = draft;
			notifications.position = ack.positions.notification;
			self.placement = None;
			true
		} else {
			false
		}
	}

	fn preview(&self) -> Option<&ActivePlacement> {
		self.placement
			.as_ref()
			.filter(|placement| placement.request.mode == Mode::Preview)
	}

	fn preview_mut(&mut self) -> Option<&mut ActivePlacement> {
		self.placement
			.as_mut()
			.filter(|placement| placement.request.mode == Mode::Preview)
	}

	fn previewing(&self) -> bool {
		self.preview().is_some()
	}

	pub fn preview_appearance(&self) -> Option<&Appearance> {
		self.draft
			.as_ref()
			.filter(|_| self.previewing())
			.map(|draft| &draft.appearance)
	}

	fn draft(&mut self) -> &mut Layout {
		self.draft.get_or_insert_with(|| self.saved.clone())
	}

	fn start(&mut self, mode: Mode, notifications: &NotificationSettings) {
		let Some(monitor) = self.layout().monitor.clone() else {
			return;
		};
		if !self.placement_supported(&monitor, mode) {
			return;
		}
		let Some(display) = self.display(&monitor) else {
			return;
		};
		let output = (display.id, display.logical_size);
		let (initial, display_before_preview) = if mode == Mode::Preview
			&& let Some(preview) = self.preview()
		{
			(
				preview
					.ack
					.as_ref()
					.filter(|ack| !ack.dragging)
					.map_or(preview.request.initial, |ack| ack.positions),
				preview.display_before_preview.clone(),
			)
		} else {
			(
				LayoutPositions {
					voice: self.saved.position,
					notification: notifications.position,
				},
				(mode == Mode::Preview).then(|| monitor.clone()),
			)
		};
		if mode == Mode::Preview {
			self.draft();
			self.save_requested = false;
			self.layout_interrupted = false;
		}
		self.generation = self.generation.wrapping_add(1);
		self.placement = Some(ActivePlacement {
			request: Placement {
				generation: self.generation,
				monitor,
				initial,
				corner: notifications.corner,
				mode,
				command: None,
			},
			output,
			display_before_preview,
			ack: None,
			sequence: 0,
		});
	}

	fn picker_available(&self) -> bool {
		self.available()
			&& self.preview().is_none_or(|preview| {
				preview.request.command.is_none()
					&& preview.ack.as_ref().is_none_or(|ack| !ack.dragging)
			})
	}

	fn select(&mut self, monitor: String, now: Instant, notifications: &NotificationSettings) {
		if self.picker_available()
			&& self
				.display(&monitor)
				.is_some_and(|display| self.display_choice_enabled(display))
			&& self.layout().monitor.as_ref() != Some(&monitor)
		{
			self.draft().monitor = Some(monitor);
			if self.previewing() {
				self.start(Mode::Preview, notifications);
			} else {
				self.placement = None;
				self.start(Self::identify(now), notifications);
			}
		}
	}

	fn queue_layout(&mut self, action: LayoutAction) {
		if let Some(placement) = self.preview_mut()
			&& let Some(ack) = placement.commandable_ack()
			&& (match action {
				LayoutAction::Save { revision } => {
					ack.status == LayoutStatus::Ready && ack.revision == revision
				}
				LayoutAction::Nudge { .. } | LayoutAction::Reset(_) => true,
			}) {
			placement.sequence += 1;
			placement.request.command = Some(LayoutCommand {
				sequence: placement.sequence,
				action,
			});
		}
	}

	fn identify(now: Instant) -> Mode {
		Mode::Identify(now + Duration::from_secs(3))
	}

	fn interrupt_placement(&mut self) {
		if let Some(preview) = self
			.placement
			.take()
			.filter(|placement| placement.request.mode == Mode::Preview)
		{
			self.layout_interrupted = true;
			self.save_requested = false;
			if let Some(draft) = &mut self.draft {
				draft.monitor = preview.display_before_preview;
				if *draft == self.saved {
					self.draft = None;
				}
			}
		}
	}

	pub fn cancel(&mut self) {
		self.draft = None;
		self.save_requested = false;
		self.placement = None;
		self.layout_interrupted = false;
	}

	fn can_save_preferences(&self) -> bool {
		!self.previewing()
			&& self.draft.as_ref().is_some_and(|draft| {
				(draft.monitor == self.saved.monitor
					&& draft.appearance == self.saved.appearance
					&& draft.roster != self.saved.roster)
					|| (self.available()
						&& draft
							.monitor
							.as_deref()
							.and_then(|monitor| self.display(monitor))
							.is_some_and(Display::selectable))
			})
	}

	/// Placement changes require a native approval through `apply_layout` instead.
	pub fn save_if_requested(&mut self) -> bool {
		if std::mem::take(&mut self.save_requested)
			&& self.can_save_preferences()
			&& let Some(draft) = self.draft.take()
		{
			self.saved = draft;
			self.placement = None;
			self.layout_interrupted = false;
			true
		} else {
			false
		}
	}

	pub(super) fn show_layout(
		&mut self,
		ui: &mut egui::Ui,
		language: Language,
		notifications: &NotificationSettings,
	) {
		let text = |key| language.text(key);
		let monitor = self.layout().monitor.as_deref();
		let display = monitor.and_then(|name| self.display(name));
		let label = ui.label(text("voice-overlay-display"));
		let caption = monitor.map_or_else(
			|| text("voice-overlay-no-selection"),
			|monitor| display.map_or_else(|| monitor.to_owned(), Display::caption),
		);
		let mut selected = None;
		ui.add_enabled_ui(
			self.picker_available()
				&& self
					.displays
					.iter()
					.any(|display| self.display_choice_enabled(display)),
			|ui| {
				egui::ComboBox::from_id_salt("voice-overlay-display")
					.selected_text(caption)
					.width(ui.available_width())
					.wrap()
					.show_ui(ui, |ui| {
						for display in &self.displays {
							let row = ui
								.add_enabled(
									self.display_choice_enabled(display),
									egui::Button::selectable(
										monitor == Some(display.name.as_str()),
										display.caption(),
									)
									.wrap(),
								)
								.on_disabled_hover_text(text(
									if self.previewing() && display.selectable() {
										"voice-overlay-layout-too-small"
									} else {
										"overlay-display-unsupported"
									},
								));
							if row.clicked() {
								selected = Some(display.name.clone());
							}
						}
					})
					.response
					.labelled_by(label.id);
			},
		);
		match self.availability {
			Availability::Starting => {
				ui.weak(text("voice-overlay-starting"));
			}
			Availability::Unavailable => {
				ui.weak(text("voice-overlay-unavailable"));
			}
			Availability::EditingUnavailable | Availability::Available => {
				if monitor.is_some() && display.is_none() {
					ui.weak(text("voice-overlay-disconnected"));
				} else if self.displays.is_empty() {
					ui.weak(text("voice-overlay-no-displays"));
				}
				if let Some(display) = display {
					let status = match (display.live_supported, display.notifications_supported) {
						(true, true) => None,
						(true, false) => Some("overlay-notifications-display-unavailable"),
						(false, true) => Some("voice-overlay-display-unavailable"),
						(false, false) => Some("overlay-display-unsupported"),
					};
					if let Some(status) = status {
						ui.weak(text(status));
					}
					if self.availability == Availability::Available
						&& display.selectable()
						&& !display.layout_supported
					{
						ui.weak(text("voice-overlay-layout-too-small"));
					}
				} else if !self.displays.is_empty()
					&& !self.displays.iter().any(Display::selectable)
				{
					ui.weak(text("overlay-no-supported-displays"));
				}
				if self.availability == Availability::EditingUnavailable {
					ui.weak(text("voice-overlay-editing-unavailable"));
				}
			}
		}
		let now = Instant::now();
		if let Some(selected) = selected {
			self.select(selected, now, notifications);
		}
		if self.previewing() {
			self.show_layout_editor(ui, language);
		} else {
			if self.layout_interrupted {
				ui.weak(text("voice-overlay-layout-interrupted"));
			}
			let monitor = self.layout().monitor.as_deref();
			let identify = Self::identify(now);
			let identify_supported =
				monitor.is_some_and(|name| self.placement_supported(name, identify));
			let edit_supported =
				monitor.is_some_and(|name| self.placement_supported(name, Mode::Preview));
			ui.horizontal_wrapped(|ui| {
				if ui
					.add_enabled(
						identify_supported,
						egui::Button::new(text("voice-overlay-identify")),
					)
					.clicked()
				{
					self.start(identify, notifications);
				}
				ui.add_enabled_ui(edit_supported, |ui| {
					let kind = if self.draft.is_some() {
						design::ButtonKind::Outline
					} else {
						design::ButtonKind::Primary
					};
					if design::button(ui, "voice-overlay-edit", kind).clicked() {
						self.start(Mode::Preview, notifications);
					}
				});
			});
			if self.draft.is_some() && !self.previewing() {
				ui.weak(text("voice-overlay-draft-pending"));
				ui.horizontal_wrapped(|ui| {
					ui.add_enabled_ui(self.can_save_preferences(), |ui| {
						if design::button(
							ui,
							"voice-overlay-save-preferences",
							design::ButtonKind::Primary,
						)
						.clicked()
						{
							self.save_requested = true;
						}
					});
					if design::button(ui, "voice-overlay-cancel", design::ButtonKind::Neutral)
						.clicked()
					{
						self.cancel();
					}
				});
			}
		}
		show_persistence(
			ui,
			language,
			self.layout_persistence,
			&mut self.layout_retry_requested,
		);
	}

	fn show_layout_editor(&mut self, ui: &mut egui::Ui, language: Language) {
		let Some(preview) = self.preview() else {
			return;
		};
		let text = |key| language.text(key);
		ui.weak(text("voice-overlay-preview-help"));
		if preview.request.command.is_some() || preview.ack.is_none() {
			ui.weak(text("voice-overlay-layout-waiting"));
		}
		if let Some(ack) = &preview.ack {
			if ack.dragging {
				ui.weak(text("voice-overlay-layout-dragging"));
			}
			match ack.status {
				LayoutStatus::Ready => {}
				LayoutStatus::Overlap => {
					ui.label(text("voice-overlay-layout-overlap"));
				}
				LayoutStatus::TooSmall => {
					ui.label(text("voice-overlay-layout-too-small"));
				}
			}
		}
		let can_move = preview.movable_ack().is_some();
		let save_revision = preview
			.commandable_ack()
			.filter(|ack| ack.status == LayoutStatus::Ready)
			.map(|ack| ack.revision);
		let controls_id = ui.make_persistent_id("voice-overlay-layout-controls");
		ui.scope_builder(egui::UiBuilder::new().scope_id(controls_id), |ui| {
			for (panel, panel_label, reset_label) in [
				(
					LayoutPanel::Voice,
					"overlay-voice-roster",
					"voice-overlay-reset",
				),
				(
					LayoutPanel::Notification,
					"voice-overlay-layout-notification",
					"voice-overlay-reset-notification",
				),
			] {
				ui.label(text(panel_label));
				ui.horizontal_wrapped(|ui| {
					for (x, y, label) in [
						(-1, 0, "voice-overlay-layout-move-left"),
						(1, 0, "voice-overlay-layout-move-right"),
						(0, -1, "voice-overlay-layout-move-up"),
						(0, 1, "voice-overlay-layout-move-down"),
					] {
						if ui
							.add_enabled(can_move, egui::Button::new(text(label)))
							.clicked()
						{
							self.queue_layout(LayoutAction::Nudge { panel, x, y });
						}
					}
					if ui
						.add_enabled(can_move, egui::Button::new(text(reset_label)))
						.clicked()
					{
						self.queue_layout(LayoutAction::Reset(panel));
					}
				});
			}
			ui.horizontal_wrapped(|ui| {
				ui.add_enabled_ui(save_revision.is_some(), |ui| {
					if design::button(ui, "voice-overlay-save", design::ButtonKind::Primary)
						.clicked() && let Some(revision) = save_revision
					{
						self.queue_layout(LayoutAction::Save { revision });
					}
				});
				if design::button(ui, "voice-overlay-cancel", design::ButtonKind::Neutral).clicked()
				{
					self.cancel();
				}
			});
		});
	}

	pub(super) fn show_roster(&mut self, ui: &mut egui::Ui, language: Language) {
		let text = |key| language.text(key);
		ui.add_enabled_ui(self.available(), |ui| {
			design::switch(ui, &text("voice-overlay-enable"), None, &mut self.enabled);
			let saving_layout = self
				.placement()
				.and_then(|placement| placement.command.as_ref())
				.is_some_and(|command| matches!(command.action, LayoutAction::Save { .. }));
			ui.add_enabled_ui(!saving_layout, |ui| {
				ui.label(text("voice-overlay-roster"));
				let mut roster = self.layout().roster;
				ui.radio_value(
					&mut roster,
					RosterMode::SpeakingOnly,
					text("voice-overlay-speaking-only"),
				);
				ui.radio_value(
					&mut roster,
					RosterMode::Everyone,
					text("voice-overlay-everyone"),
				);
				if roster != self.layout().roster {
					self.draft().roster = roster;
				}
			});
		});
		if self.paused_after_move() {
			ui.label(text("voice-overlay-paused-after-move"));
		}
		show_status_legend(ui, language);
		egui::CollapsingHeader::new(text("voice-overlay-appearance"))
			.id_salt("voice-overlay-appearance")
			.show(ui, |ui| {
				if let Some(preview) = self.preview() {
					let editable = preview.request.command.is_none()
						&& preview.ack.as_ref().is_none_or(|ack| !ack.dragging);
					if let Some(draft) = &mut self.draft {
						ui.add_enabled_ui(editable, |ui| {
							show_appearance(ui, language, &self.fonts, &mut draft.appearance);
						});
					}
				} else {
					ui.weak(text("voice-overlay-appearance-help"));
				}
			});
		if self.persistence != self.layout_persistence {
			show_persistence(ui, language, self.persistence, &mut self.retry_requested);
		}
	}
}

fn show_status_legend(ui: &mut egui::Ui, language: Language) {
	let colors = design::palette(ui);
	ui.label(language.text("voice-overlay-status-legend"));
	ui.horizontal_wrapped(|ui| {
		for (icon, color, label) in [
			(None, colors.positive, "voice-overlay-status-speaking"),
			(
				Some(icons::Icon::MicrophoneSlash),
				colors.muted,
				"voice-voice-participant-microphone-muted",
			),
			(
				Some(icons::Icon::HeadphonesSlash),
				colors.muted,
				"voice-voice-participant-deafened",
			),
			(
				Some(icons::Icon::Speaker),
				colors.danger,
				"voice-voice-participant-muted-for-you-on-this-device",
			),
		] {
			ui.horizontal(|ui| {
				let (rect, _) =
					ui.allocate_exact_size(egui::Vec2::splat(20.0), egui::Sense::hover());
				if let Some(icon) = icon {
					icons::paint(ui.painter(), icon, rect.shrink(1.0), color);
				} else {
					ui.painter().circle_filled(rect.center(), 4.0, color);
				}
				ui.add(egui::Label::new(language.text(label)).wrap());
			});
		}
	});
}

pub(super) fn show_persistence(
	ui: &mut egui::Ui,
	language: Language,
	persistence: Persistence,
	retry_requested: &mut bool,
) {
	let text = |key| language.text(key);
	match persistence {
		Persistence::Idle => {}
		Persistence::Saving => {
			ui.weak(text("voice-overlay-saving"));
		}
		Persistence::NotLoaded => {
			ui.label(text("voice-overlay-save-not-loaded"));
		}
		Persistence::Failed { retry } => {
			ui.label(text("voice-overlay-save-failed"));
			if retry && ui.button(text("voice-overlay-retry-save")).clicked() {
				*retry_requested = true;
			}
		}
	}
}

fn show_appearance(
	ui: &mut egui::Ui,
	language: Language,
	fonts: &[String],
	style: &mut Appearance,
) {
	let text = |key| language.text(key);
	design::switch(
		ui,
		&text("voice-overlay-avatars"),
		None,
		&mut style.show_avatars,
	);
	if style.show_avatars {
		design::slider_row(
			ui,
			"voice-overlay-avatar-size",
			None,
			&mut style.avatar_size,
			Appearance::AVATAR_SIZES,
			" px",
		);
	}
	let font_label = ui.label(text("voice-overlay-font"));
	let default_font = text("voice-overlay-font-default");
	egui::ComboBox::from_id_salt("voice-overlay-font")
		.selected_text(style.font_family.as_deref().unwrap_or(&default_font))
		.width(ui.available_width())
		.wrap()
		.show_ui(ui, |ui| {
			ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
			if ui
				.selectable_label(style.font_family.is_none(), &default_font)
				.clicked()
			{
				style.font_family = None;
			}
			for family in fonts {
				if ui
					.selectable_label(style.font_family.as_ref() == Some(family), family)
					.clicked()
				{
					style.font_family = Some(family.clone());
				}
			}
		})
		.response
		.labelled_by(font_label.id);
	if style
		.font_family
		.as_ref()
		.is_some_and(|family| fonts.binary_search(family).is_err())
	{
		ui.weak(text("voice-overlay-font-missing"));
	}
	design::slider_row(
		ui,
		"voice-overlay-font-size",
		None,
		&mut style.font_size,
		Appearance::FONT_SIZES,
		" pt",
	);
	let weight_label = |weight| {
		text(match weight {
			FontWeight::Normal => "voice-overlay-weight-normal",
			FontWeight::Semibold => "voice-overlay-weight-semibold",
			FontWeight::Bold => "voice-overlay-weight-bold",
		})
	};
	let weight_caption = ui.label(text("voice-overlay-font-weight"));
	egui::ComboBox::from_id_salt("voice-overlay-font-weight")
		.selected_text(weight_label(style.font_weight))
		.width(ui.available_width())
		.wrap()
		.show_ui(ui, |ui| {
			ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
			for weight in [FontWeight::Normal, FontWeight::Semibold, FontWeight::Bold] {
				ui.selectable_value(&mut style.font_weight, weight, weight_label(weight));
			}
		})
		.response
		.labelled_by(weight_caption.id);
	for (label, color) in [
		("voice-overlay-text-color", &mut style.text),
		("voice-overlay-background-color", &mut style.background),
		("voice-overlay-accent-color", &mut style.accent),
	] {
		ui.label(text(label));
		ui.horizontal(|ui| design::color_edit(ui, color));
	}
	design::slider_row(
		ui,
		"voice-overlay-background-opacity",
		None,
		&mut style.background_opacity,
		0..=100,
		"%",
	);
	if design::button(
		ui,
		"voice-overlay-reset-appearance",
		design::ButtonKind::Neutral,
	)
	.clicked()
	{
		*style = Appearance::default();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn displays(names: &[&str]) -> Vec<Display> {
		names
			.iter()
			.enumerate()
			.map(|(index, &name)| Display {
				id: index as u32 + 1,
				name: name.into(),
				label: "Identical displays".into(),
				logical_size: (1920, 1080),
				live_supported: true,
				notifications_supported: true,
				layout_supported: true,
			})
			.collect()
	}

	fn ready_ack(revision: u64, command: u64) -> LayoutAck {
		LayoutAck {
			revision,
			command,
			positions: LayoutPositions {
				voice: Some(Position { x: 1000, y: 2000 }),
				notification: Some(Position { x: 6000, y: 5000 }),
			},
			dragging: false,
			status: LayoutStatus::Ready,
		}
	}

	#[test]
	fn live_only_display_draft_saves_without_placement_protocols() {
		let mut settings = Settings::default();
		let now = Instant::now();
		let notifications = NotificationSettings::default();
		settings.set_displays(Availability::EditingUnavailable, displays(&["DP-1"]));
		settings.select("DP-1".into(), now, &notifications);
		assert!(settings.placement().is_none());
		settings.start(Settings::identify(now), &notifications);
		settings.start(Mode::Preview, &notifications);
		assert!(settings.placement().is_none());
		assert_eq!(settings.saved.monitor, None);

		let mut unsupported = displays(&["DP-1"]);
		unsupported[0].live_supported = false;
		unsupported[0].notifications_supported = false;
		unsupported[0].layout_supported = false;
		settings.set_displays(Availability::EditingUnavailable, unsupported);
		settings.save_requested = true;
		assert!(!settings.save_if_requested());

		settings.set_displays(Availability::EditingUnavailable, displays(&["DP-1"]));
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		assert_eq!(settings.saved.monitor.as_deref(), Some("DP-1"));
		assert!(settings.draft.is_none());

		let mut notifications_only = displays(&["DP-2"]);
		notifications_only[0].live_supported = false;
		notifications_only[0].layout_supported = false;
		settings.set_displays(Availability::Available, notifications_only);
		settings.select("DP-2".into(), now, &notifications);
		settings.start(Mode::Preview, &notifications);
		assert!(settings.placement().is_none());
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		assert_eq!(settings.saved.monitor.as_deref(), Some("DP-2"));
		assert!(!settings.live_supported("DP-2") && !settings.enabled);
	}

	#[test]
	fn paired_save_requires_native_approval_of_the_latest_ready_revision() {
		let mut settings = Settings::default();
		let mut notifications = NotificationSettings {
			include_excerpts: true,
			corner: ToastCorner::BottomLeft,
			..NotificationSettings::default()
		};
		settings.set_displays(Availability::Available, displays(&["DP-1"]));
		settings.select("DP-1".into(), Instant::now(), &notifications);
		settings.start(Mode::Preview, &notifications);
		let generation = settings.placement().unwrap().generation;
		assert_eq!(
			settings.placement().unwrap().corner,
			ToastCorner::BottomLeft
		);
		settings.draft().appearance.show_avatars = false;
		let save = |revision| LayoutAction::Save { revision };
		settings.queue_layout(save(1));
		assert!(settings.placement().unwrap().command.is_none());
		settings.report_layout(
			generation,
			"DP-1",
			LayoutAck {
				status: LayoutStatus::Overlap,
				..ready_ack(1, 0)
			},
		);
		settings.queue_layout(save(1));
		assert!(settings.placement().unwrap().command.is_none());

		settings.queue_layout(LayoutAction::Nudge {
			panel: LayoutPanel::Notification,
			x: 1,
			y: 0,
		});
		settings.queue_layout(LayoutAction::Reset(LayoutPanel::Voice));
		let pending = settings.placement().unwrap().command.as_ref().unwrap();
		assert_eq!(pending.sequence, 1);
		assert!(matches!(
			pending.action,
			LayoutAction::Nudge {
				panel: LayoutPanel::Notification,
				..
			}
		));
		settings.report_layout(generation, "DP-1", ready_ack(2, 1));
		settings.queue_layout(save(2));
		settings.save_requested = true;
		assert!(!settings.save_if_requested());
		assert!(!settings.apply_layout(generation, "DP-1", ready_ack(1, 2), &mut notifications));
		assert!(!settings.apply_layout(generation, "DP-1", ready_ack(2, 1), &mut notifications));
		settings.report_layout(
			generation,
			"DP-1",
			LayoutAck {
				dragging: true,
				..ready_ack(3, 1)
			},
		);
		assert!(settings.placement().unwrap().command.is_some());
		assert!(!settings.apply_layout(generation, "DP-1", ready_ack(2, 2), &mut notifications));

		// An ordinary update acknowledging the Save is a rejection, not an approval.
		settings.report_layout(generation, "DP-1", ready_ack(4, 2));
		assert!(settings.placement().unwrap().command.is_none());
		assert_eq!(notifications.position, None);
		assert_eq!(settings.saved, Layout::default());
		settings.queue_layout(save(4));
		let approved = ready_ack(4, 3);
		let positions = approved.positions;
		assert!(settings.apply_layout(generation, "DP-1", approved, &mut notifications));
		assert_eq!(settings.saved.monitor.as_deref(), Some("DP-1"));
		assert_eq!(settings.saved.position, positions.voice);
		assert!(!settings.saved.appearance.show_avatars);
		assert_eq!(notifications.position, positions.notification);
		assert!(notifications.include_excerpts);
		assert_eq!(notifications.corner, ToastCorner::BottomLeft);
		assert!(settings.placement().is_none());
	}

	#[test]
	fn display_switch_carries_released_positions_and_cancel_rejects_late_approval() {
		let mut settings = Settings::default();
		let mut notifications = NotificationSettings::default();
		let now = Instant::now();
		settings.set_displays(Availability::Available, displays(&["DP-1", "DP-2"]));
		settings.select("DP-1".into(), now, &notifications);
		settings.start(Mode::Preview, &notifications);
		let old = settings.placement().unwrap().generation;
		settings.report_layout(
			old,
			"DP-1",
			LayoutAck {
				dragging: true,
				..ready_ack(1, 0)
			},
		);
		settings.select("DP-2".into(), now, &notifications);
		assert_eq!(settings.placement().unwrap().monitor, "DP-1");
		let released = ready_ack(2, 0);
		let positions = released.positions;
		settings.report_layout(old, "DP-1", released);
		settings.select("DP-2".into(), now, &notifications);
		let placement = settings.placement().unwrap();
		assert_eq!(placement.initial, positions);
		let current = placement.generation;
		settings.report_closed(old, "DP-1");
		settings.report_layout(old, "DP-1", ready_ack(20, 0));
		settings.queue_layout(LayoutAction::Save { revision: 20 });
		assert!(settings.placement().unwrap().command.is_none());
		settings.report_layout(current, "DP-2", ready_ack(1, 0));
		settings.report_layout(current, "DP-2", ready_ack(0, 0));
		settings.queue_layout(LayoutAction::Save { revision: 1 });
		assert_eq!(
			settings
				.placement()
				.unwrap()
				.command
				.as_ref()
				.unwrap()
				.sequence,
			1
		);
		let approved = ready_ack(1, 1);
		assert!(!settings.apply_layout(old, "DP-1", approved, &mut notifications));
		// Handle the same frame's Cancel before considering the pending approval.
		settings.cancel();
		assert!(!settings.apply_layout(current, "DP-2", approved, &mut notifications));
		assert_eq!(settings.saved, Layout::default());
		assert_eq!(notifications.position, None);
	}

	#[test]
	fn roster_only_draft_saves_without_enabling_voice_or_changing_placement() {
		let mut settings = Settings::default();
		settings.draft().roster = RosterMode::Everyone;
		settings.cancel();
		assert_eq!(settings.saved.roster, RosterMode::SpeakingOnly);
		settings.draft().roster = RosterMode::Everyone;
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		assert_eq!(settings.saved.roster, RosterMode::Everyone);
		assert!(
			settings.saved.monitor.is_none()
				&& settings.saved.position.is_none()
				&& !settings.enabled
		);
	}

	#[test]
	fn interrupted_preview_discards_only_its_display_change() {
		let mut settings = Settings::default();
		let mut notifications = NotificationSettings {
			position: Some(Position { x: 5000, y: 6000 }),
			..NotificationSettings::default()
		};
		let now = Instant::now();
		let mut outputs = displays(&["DP-1", "DP-2"]);
		settings.set_displays(Availability::Available, outputs.clone());
		settings.saved.position = Some(Position { x: 2000, y: 1000 });
		let saved_voice = settings.saved.position;
		let saved_notification = notifications.position;
		// This explicit live selection predates Preview and is safe to retain.
		settings.select("DP-1".into(), now, &notifications);
		settings.start(Mode::Preview, &notifications);
		settings.select("DP-2".into(), now, &notifications);
		settings.draft().roster = RosterMode::Everyone;
		settings.draft().appearance.show_avatars = false;
		let generation = settings.placement().unwrap().generation;
		settings.report_layout(generation, "DP-2", ready_ack(1, 0));
		settings.queue_layout(LayoutAction::Save { revision: 1 });
		assert!(settings.placement().unwrap().command.is_some());

		outputs[1].logical_size.0 += 1;
		settings.set_displays(Availability::Available, outputs.clone());
		assert!(settings.placement().is_none());
		assert!(!settings.apply_layout(generation, "DP-2", ready_ack(1, 1), &mut notifications));
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		assert_eq!(settings.saved.monitor.as_deref(), Some("DP-1"));
		assert_eq!(settings.saved.roster, RosterMode::Everyone);
		assert!(!settings.saved.appearance.show_avatars);
		assert_eq!(settings.saved.position, saved_voice);
		assert_eq!(notifications.position, saved_notification);

		settings.set_displays(Availability::EditingUnavailable, outputs);
		settings.select("DP-2".into(), now, &notifications);
		assert!(settings.placement().is_none());
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		assert_eq!(settings.saved.monitor.as_deref(), Some("DP-2"));
		assert_eq!(settings.saved.position, saved_voice);
		assert_eq!(notifications.position, saved_notification);
	}
}
