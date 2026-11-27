use crate::{design, i18n::Language, voice_overlay};
use model::voice_overlay::NotificationSettings;
use std::time::Duration;

pub const TOAST_LIFETIME: Duration = Duration::from_secs(4);

#[derive(Default)]
pub struct Settings {
	pub saved: NotificationSettings,
	pub persistence: voice_overlay::Persistence,
	retry_requested: bool,
}

impl Settings {
	pub fn take_retry_request(&mut self) -> bool {
		std::mem::take(&mut self.retry_requested)
	}

	pub(super) fn show(
		&mut self,
		ui: &mut egui::Ui,
		language: Language,
		voice: &voice_overlay::Settings,
	) {
		let text = |key| language.text(key);
		let saved = self.saved;
		let enabled = design::switch(
			ui,
			&text("overlay-notifications-enable"),
			None,
			&mut self.saved.enabled,
		);
		let routing = ui.add(egui::Label::new(text("overlay-notifications-routing")).wrap());
		if ui.ctx().has_accesskit_node(enabled.id) {
			ui.ctx().accesskit_node_builder(enabled.id, |node| {
				node.push_described_by(routing.id.accesskit_id());
			});
		}
		design::switch(
			ui,
			&text("overlay-notifications-excerpts"),
			None,
			&mut self.saved.include_excerpts,
		);
		ui.label(text("overlay-notifications-capture"));
		if saved != self.saved {
			ui.ctx().request_repaint();
		}
		// A single preferences write covers the layout and both features.
		if self.persistence != voice.layout_persistence && self.persistence != voice.persistence {
			voice_overlay::show_persistence(
				ui,
				language,
				self.persistence,
				&mut self.retry_requested,
			);
		}
	}
}
