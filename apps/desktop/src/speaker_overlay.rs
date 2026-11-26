use eframe::egui;
use model::Id;
use platform::speaker_layer::{self as layer, AVATAR_EDGE, MAX_ROWS};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallKey {
	pub generation: u64,
	pub channel: Id,
	pub request: u64,
	pub negotiation_revision: u64,
	pub media_epoch: u64,
}

#[derive(Default)]
struct Avatar {
	requested: bool,
	pixels: Option<Arc<[u8]>>,
}

pub(crate) struct Overlay {
	native: Option<layer::Overlay>,
	avatars: HashMap<String, Avatar>,
}

impl Overlay {
	pub(crate) fn new(ctx: &egui::Context) -> Self {
		let native = std::env::var_os("WAYLAND_DISPLAY").and_then(|_| {
			let ctx = ctx.clone();
			layer::Overlay::start(move || ctx.request_repaint())
		});
		Self {
			native,
			avatars: HashMap::new(),
		}
	}

	pub(crate) fn disable(&mut self) {
		if let Some(native) = &self.native {
			native.update(layer::Frame::default());
		}
		self.avatars.clear();
	}

	pub(crate) fn accept_avatar(&mut self, key: &str, image: Option<&egui::ColorImage>) {
		let Some(avatar) = self.avatars.get_mut(key) else {
			return;
		};
		if let Some(image) = image {
			let [w, h] = image.resolution();
			if w == 0 || h == 0 {
				return;
			}
			let edge = w.min(h);
			let mut result = Vec::with_capacity(AVATAR_EDGE * AVATAR_EDGE * 4);
			for y in 0..AVATAR_EDGE {
				for x in 0..AVATAR_EDGE {
					let offset = ((h - edge) / 2 + y * edge / AVATAR_EDGE) * w
						+ (w - edge) / 2 + x * edge / AVATAR_EDGE;
					let [r, g, b, a] = image.pixels()[offset].to_array();
					result.extend_from_slice(&u32::from_be_bytes([a, r, g, b]).to_ne_bytes());
				}
			}
			avatar.pixels = Some(result.into());
		}
	}

	pub(crate) fn publish(
		&mut self,
		state: &client_core::State,
		ui: &mut ui::MessagingUi,
		voice: &crate::voice::Voice,
		worker: Option<&crate::avatars::AvatarWorker>,
	) {
		let Some(native) = &mut self.native else {
			return;
		};
		{
			let status = native.status.borrow_and_update();
			if status.has_changed() {
				ui.voice_overlay_monitors = match &*status {
					layer::Status::Available(monitors) => monitors
						.iter()
						.map(|m| (m.name.clone(), m.label.clone()))
						.collect(),
					_ => Vec::new(),
				};
			}
		}
		let monitor = ui
			.voice_overlay_monitor
			.as_ref()
			.filter(|name| ui.voice_overlay_monitors.iter().any(|(id, _)| id == *name))
			.cloned();
		let mut rows = Vec::new();
		if monitor.is_some() {
			if state.demo {
				rows.push((
					Id(1),
					"Demo speaker".into(),
					state.user.as_ref().map(model::User::avatar_key),
				));
			} else if state.auth == client_core::auth::AuthState::Authenticated
				&& state.gateway_connected
				&& let Some((_, speakers)) = voice.overlay_source(state)
			{
				for (id, name, user) in ui.active_voice_speakers(state, &speakers) {
					let Err(index) = rows.binary_search_by_key(&id, |(id, _, _)| *id) else {
						continue;
					};
					if index == MAX_ROWS {
						continue;
					}
					if rows.len() == MAX_ROWS {
						rows.pop();
					}
					let name = name
						.chars()
						.filter(|ch| !ch.is_control())
						.take(80)
						.collect();
					rows.insert(index, (id, name, user.map(model::User::avatar_key)));
				}
			}
		}
		// Retain recent avatars through brief silences, without growing beyond two full overlays.
		if monitor.is_none() {
			self.avatars.clear();
		} else if self.avatars.len() > MAX_ROWS {
			self.avatars.retain(|key, _| {
				rows.iter()
					.any(|(_, _, active)| active.as_ref() == Some(key))
			});
		}
		let rows = rows
			.into_iter()
			.map(|(_, name, key)| {
				let avatar = key.and_then(|key| {
					let avatar = self.avatars.entry(key.clone()).or_default();
					if !avatar.requested
						&& let Some(worker) = worker
					{
						avatar.requested = worker.request(key);
					}
					avatar.pixels.clone()
				});
				layer::Speaker { name, avatar }
			})
			.collect();
		native.update(layer::Frame { monitor, rows });
	}
}

#[test]
fn only_visible_active_speakers_are_listed_with_their_server_nicknames() {
	let mut state = test_support::voice_demo_state();
	state.voice.roster[0].member.as_mut().unwrap().nick = Some("Server nickname".into());
	let ui = ui::MessagingUi::default();
	// A muted member in the same call, a member of another call, and an unknown user.
	let sample = [Id(2), Id(1), Id(4), Id(999)];
	let rows: Vec<_> = ui.active_voice_speakers(&state, &sample).collect();
	assert_eq!(rows.len(), 1);
	assert_eq!(
		(rows[0].0, rows[0].1, rows[0].2.unwrap().id),
		(Id(1), "Server nickname", Id(1))
	);
	state.voice.active.as_mut().unwrap().deafened = true;
	assert!(ui.active_voice_speakers(&state, &sample).next().is_none());
	state.voice.active.as_mut().unwrap().deafened = false;
	state.channels.retain(|channel| channel.id != Id(25));
	assert!(ui.active_voice_speakers(&state, &sample).next().is_none());
}
