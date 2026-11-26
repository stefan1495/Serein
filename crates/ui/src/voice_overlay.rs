use crate::{design, i18n::Language};
use model::voice_overlay::{Appearance, FontWeight, Position};
use std::{
	sync::Arc,
	time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
	pub monitor: Option<String>,
	pub position: Option<Position>,
	pub appearance: Appearance,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Display {
	pub name: String,
	pub label: String,
}

impl Display {
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
	pub initial: Option<Position>,
	pub mode: Mode,
}

#[derive(Default)]
pub struct Settings {
	pub enabled: bool,
	pub saved: Layout,
	pub fonts: Arc<[String]>,
	availability: Availability,
	displays: Vec<Display>,
	draft: Option<Layout>,
	placement: Option<Placement>,
	generation: u64,
	dragging: bool,
	save_requested: bool,
}

impl Settings {
	fn layout(&self) -> &Layout {
		self.draft.as_ref().unwrap_or(&self.saved)
	}

	pub fn set_displays(&mut self, availability: Availability, displays: Vec<Display>) {
		self.availability = availability;
		self.displays = displays;
		if self.placement.as_ref().is_some_and(|placement| {
			availability != Availability::Available || !self.connected(&placement.monitor)
		}) {
			self.stop_placement();
		}
	}

	pub fn placement(&self) -> Option<&Placement> {
		self.placement.as_ref()
	}

	pub fn connected(&self, monitor: &str) -> bool {
		self.displays.iter().any(|display| display.name == monitor)
	}

	fn matches(&self, generation: u64, monitor: &str) -> bool {
		self.placement
			.as_ref()
			.is_some_and(|current| current.generation == generation && current.monitor == monitor)
	}

	pub fn report_closed(&mut self, generation: u64, monitor: &str) {
		if self.matches(generation, monitor) {
			let preview = self.previewing();
			self.stop_placement();
			if preview {
				self.save_requested = false;
			}
		}
	}

	pub fn report_drag(
		&mut self,
		generation: u64,
		monitor: &str,
		position: Option<Position>,
		dragging: bool,
	) {
		if self.matches(generation, monitor) && self.previewing() {
			self.dragging = dragging;
			if !dragging && let Some(draft) = &mut self.draft {
				draft.position = position;
			}
		}
	}

	fn previewing(&self) -> bool {
		self.placement
			.as_ref()
			.is_some_and(|placement| placement.mode == Mode::Preview)
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

	fn start(&mut self, mode: Mode) {
		let layout = self.layout();
		let Some(monitor) = layout.monitor.clone().filter(|name| self.connected(name)) else {
			return;
		};
		let initial = layout.position;
		self.generation = self.generation.wrapping_add(1);
		self.dragging = false;
		self.placement = Some(Placement {
			generation: self.generation,
			monitor,
			initial,
			mode,
		});
	}

	fn select(&mut self, monitor: String, now: Instant) {
		if self.layout().monitor.as_ref() != Some(&monitor) {
			self.draft().monitor = Some(monitor);
			self.start(if self.previewing() {
				Mode::Preview
			} else {
				Self::identify(now)
			});
		}
	}

	fn identify(now: Instant) -> Mode {
		Mode::Identify(now + Duration::from_secs(3))
	}

	fn stop_placement(&mut self) {
		self.placement = None;
		self.dragging = false;
	}

	pub fn cancel(&mut self) {
		self.draft = None;
		self.save_requested = false;
		self.stop_placement();
	}

	fn can_save(&self) -> bool {
		!self.dragging
			&& self.availability == Availability::Available
			&& self
				.draft
				.as_ref()
				.and_then(|draft| draft.monitor.as_deref())
				.is_some_and(|monitor| self.connected(monitor))
	}

	/// Desktop re-reads the layer's latest drag status before honoring a Save click.
	pub fn save_if_requested(&mut self) -> bool {
		if std::mem::take(&mut self.save_requested)
			&& self.can_save()
			&& let Some(draft) = self.draft.take()
		{
			self.saved = draft;
			self.stop_placement();
			true
		} else {
			false
		}
	}

	pub(super) fn show(&mut self, ui: &mut egui::Ui, language: Language) {
		let text = |key| language.text(key);
		if matches!(
			self.availability,
			Availability::Starting | Availability::Unavailable
		) {
			ui.weak(text(if self.availability == Availability::Starting {
				"voice-overlay-starting"
			} else {
				"voice-overlay-unavailable"
			}));
			return;
		}
		design::switch(ui, &text("voice-overlay-enable"), None, &mut self.enabled);
		if self.availability == Availability::EditingUnavailable {
			ui.weak(text("voice-overlay-editing-unavailable"));
			return;
		}
		let monitor = self.layout().monitor.as_deref();
		let connected = monitor.is_some_and(|monitor| self.connected(monitor));
		if monitor.is_some() && !connected {
			ui.weak(text("voice-overlay-disconnected"));
		}
		if self.displays.is_empty() {
			ui.weak(text("voice-overlay-no-displays"));
		}
		ui.label(text("voice-overlay-display"));
		let caption = monitor.map_or_else(
			|| text("voice-overlay-no-selection"),
			|monitor| {
				self.displays
					.iter()
					.find(|display| display.name == monitor)
					.map_or_else(|| monitor.to_owned(), Display::caption)
			},
		);
		let mut selected = None;
		ui.add_enabled_ui(!self.displays.is_empty(), |ui| {
			design::Select::from_id_salt("voice-overlay-display")
				.selected_text(caption)
				.width(ui.available_width().min(400.0))
				.show_ui(ui, |ui| {
					for display in &self.displays {
						if ui
							.selectable_label(
								monitor == Some(display.name.as_str()),
								display.caption(),
							)
							.clicked()
						{
							selected = Some(display.name.clone());
						}
					}
				});
		});
		let now = Instant::now();
		if let Some(selected) = selected {
			self.select(selected, now);
		}
		let connected = self
			.layout()
			.monitor
			.as_deref()
			.is_some_and(|name| self.connected(name));
		ui.horizontal_wrapped(|ui| {
			ui.add_enabled_ui(connected && !self.previewing(), |ui| {
				if ui.button(text("voice-overlay-identify")).clicked() {
					self.start(Self::identify(now));
				}
				if ui.button(text("voice-overlay-edit")).clicked() {
					self.draft();
					self.start(Mode::Preview);
				}
			});
			if ui
				.add_enabled(
					connected && !self.dragging,
					egui::Button::new(text("voice-overlay-reset")),
				)
				.clicked()
			{
				self.draft().position = None;
				if self.previewing() {
					self.start(Mode::Preview);
				}
			}
		});
		if self.previewing() {
			ui.weak(text("voice-overlay-preview-help"));
		}
		if self.draft.is_some() {
			ui.horizontal_wrapped(|ui| {
				ui.add_enabled_ui(self.can_save(), |ui| {
					if design::button(ui, "voice-overlay-save", design::ButtonKind::Primary)
						.clicked()
					{
						self.save_requested = true;
					}
				});
				if design::button(ui, "voice-overlay-cancel", design::ButtonKind::Neutral).clicked()
				{
					self.cancel();
				}
			});
		}
		if self.previewing()
			&& let Some(draft) = &mut self.draft
		{
			ui.add_enabled_ui(!self.dragging, |ui| {
				show_appearance(ui, language, &self.fonts, &mut draft.appearance);
			});
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
	design::group(ui, "voice-overlay-appearance", |ui| {
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
		ui.label(text("voice-overlay-font"));
		let default_font = text("voice-overlay-font-default");
		design::Select::from_id_salt("voice-overlay-font")
			.selected_text(style.font_family.as_deref().unwrap_or(&default_font))
			.width(ui.available_width().min(400.0))
			.show_ui(ui, |ui| {
				if ui
					.selectable_label(style.font_family.is_none(), default_font)
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
			});
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
		ui.label(text("voice-overlay-font-weight"));
		design::Select::from_id_salt("voice-overlay-font-weight")
			.selected_text(weight_label(style.font_weight))
			.show_ui(ui, |ui| {
				for weight in [FontWeight::Normal, FontWeight::Semibold, FontWeight::Bold] {
					ui.selectable_value(&mut style.font_weight, weight, weight_label(weight));
				}
			});
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
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	fn displays(names: &[&str]) -> Vec<Display> {
		names
			.iter()
			.map(|&name| Display {
				name: name.into(),
				label: "Identical displays".into(),
			})
			.collect()
	}

	#[test]
	fn save_uses_the_current_outputs_final_drag_and_cancel_drops_the_draft() {
		let mut settings = Settings::default();
		settings.set_displays(Availability::Available, displays(&["DP-1", "DP-2"]));
		settings.select("DP-1".into(), Instant::now());
		settings.start(Mode::Preview);
		let old = settings.placement().unwrap().generation;
		settings.select("DP-2".into(), Instant::now());
		let current = settings.placement().unwrap().generation;
		let final_position = Some(Position { x: 2500, y: 7100 });
		settings.draft().appearance.show_avatars = false;
		assert!(!settings.preview_appearance().unwrap().show_avatars);
		assert_ne!(
			settings.saved.appearance,
			*settings.preview_appearance().unwrap()
		);
		assert_eq!(settings.placement().unwrap().generation, current);
		settings.report_drag(current, "DP-2", final_position, true);
		settings.report_drag(old, "DP-1", None, false);
		settings.save_requested = true;
		assert!(!settings.save_if_requested());
		settings.report_drag(current, "DP-2", final_position, false);
		settings.report_drag(old, "DP-1", None, false);
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		settings.select("DP-1".into(), Instant::now());
		settings.draft().position = None;
		settings.draft().appearance = Appearance::default();
		settings.cancel();
		assert!(settings.preview_appearance().is_none());
		settings.report_drag(current, "DP-2", None, false);
		assert_eq!(
			*settings.layout(),
			Layout {
				monitor: Some("DP-2".into()),
				position: final_position,
				appearance: Appearance {
					show_avatars: false,
					..Appearance::default()
				},
			}
		);
	}

	#[test]
	fn disconnect_cancels_drag_and_never_resumes_preview_or_saves_until_connected() {
		let mut settings = Settings::default();
		settings.set_displays(Availability::Available, displays(&["DP-1", "DP-2"]));
		settings.select("DP-2".into(), Instant::now());
		settings.start(Mode::Preview);
		let generation = settings.placement().unwrap().generation;
		settings.report_drag(
			generation,
			"DP-2",
			Some(Position { x: 2000, y: 4000 }),
			true,
		);
		settings.set_displays(Availability::Available, displays(&["DP-1"]));
		assert!(settings.placement().is_none());
		settings.save_requested = true;
		assert!(!settings.save_if_requested());
		settings.report_drag(
			generation,
			"DP-2",
			Some(Position { x: 1000, y: 2000 }),
			false,
		);
		settings.set_displays(Availability::Available, displays(&["DP-1", "DP-2"]));
		assert!(settings.placement().is_none());
		settings.save_requested = true;
		assert!(settings.save_if_requested());
		assert_eq!(
			settings.saved,
			Layout {
				monitor: Some("DP-2".into()),
				position: None,
				..Layout::default()
			}
		);
	}
}
