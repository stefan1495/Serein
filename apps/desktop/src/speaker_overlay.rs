use crate::notification_runtime::{DesktopAlert, GameGate, GameLease};
use client_core::{
	State,
	auth::AuthState,
	notifications::{ChannelAdmission, NotificationCandidate},
};
use eframe::egui;
use model::{
	Id, PresenceStatus,
	voice_overlay::{NotificationSettings, RosterMode, label},
};
use platform::{
	foreground_window::ForegroundWindow,
	speaker_layer::{self as layer, AVATAR_EDGE, MAX_ROWS},
};
use std::{
	collections::HashMap,
	sync::{Arc, OnceLock},
	time::{Duration, Instant},
};
use tokio::sync::watch;
use ui::voice_overlay::{Availability, Display, Mode};

const TOAST_LIFETIME: Duration = Duration::from_secs(4);
use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};

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

struct ToastSlot {
	toast: Arc<layer::Toast>,
	settings: NotificationSettings,
	live: LiveToast,
	fallback: Option<DesktopAlert>,
}

struct LiveToast {
	generation: u64,
	channel: Id,
	message: Id,
	admission: ChannelAdmission,
}

impl LiveToast {
	fn still_eligible(
		&self,
		state: &State,
		ui: &ui::MessagingUi,
		focused: bool,
		fixture: bool,
	) -> bool {
		!fixture
			&& !state.demo
			&& state.auth == AuthState::Authenticated
			&& self.generation == state.generation
			&& self.admission.allows(state)
			&& ui.own_presence.status != PresenceStatus::DoNotDisturb
			&& !(focused && ui.viewing_latest(self.channel))
			&& state
				.read_marker(self.channel)
				.flatten()
				.is_none_or(|read| read < self.message)
	}
}

impl ToastSlot {
	fn current(&self, now: Instant, output: Option<u32>, settings: NotificationSettings) -> bool {
		self.toast.active_at(now) && Some(self.toast.output) == output && self.settings == settings
	}

	fn notification(
		state: &State,
		ui: &ui::MessagingUi,
		candidate: &NotificationCandidate,
		serial: u64,
		output: u32,
		now: Instant,
		game: GameLease,
	) -> Self {
		let settings = ui.overlay_notifications.saved;
		let notification = &candidate.notification;
		let bounded = |value: &str, fallback| {
			let value = label(value, 80, 256);
			if value.is_empty() {
				ui.language.text(fallback)
			} else {
				value
			}
		};
		let channel = state
			.channel(notification.channel)
			.map_or("", |channel| state.conversation_name(channel));
		Self {
			settings,
			fallback: ui
				.notifications_enabled
				.then(|| DesktopAlert::pending(notification)),
			live: LiveToast {
				generation: state.generation,
				channel: notification.channel,
				message: notification.message,
				admission: notification.channel_admission(state),
			},
			toast: Arc::new(layer::Toast {
				serial,
				output,
				corner: settings.corner,
				position: settings.position,
				until: now + TOAST_LIFETIME,
				foreground_until: game.until,
				foreground_generation: game.generation,
				sender: bounded(candidate.overlay_sender(), "overlay-notifications-sender"),
				channel: bounded(channel, "overlay-notifications-channel"),
				excerpt: if settings.include_excerpts {
					candidate.overlay_excerpt().map(str::to_owned)
				} else {
					None
				},
			}),
		}
	}
}

pub(crate) struct Overlay {
	native: Option<layer::Overlay>,
	avatars: HashMap<String, Avatar>,
	monitors: Vec<layer::Monitor>,
	toast: Option<ToastSlot>,
	geometry_fallback: Option<ToastSlot>,
	toast_serial: u64,
	foreground: Option<ForegroundWindow>,
	foreground_attempted: bool,
	catalog: Option<Arc<OnceLock<discord_api::detectable::Index>>>,
}

fn receive_edit<T>(
	receiver: &mut watch::Receiver<Option<layer::EditUpdate>>,
	finish_frame: bool,
	handle: impl FnOnce(&layer::EditUpdate) -> T,
) -> Option<T> {
	if finish_frame {
		let update = receiver.borrow_and_update();
		return update.as_ref().filter(|_| update.has_changed()).map(handle);
	}
	if receiver.has_changed().is_ok_and(|changed| !changed) {
		return None;
	}
	// Advance a copy so logic can process layout/closure without marking an approval seen.
	let mut early = receiver.clone();
	let update = {
		let update = early.borrow_and_update();
		if matches!(
			update.as_ref().map(|edit| edit.state),
			Some(layer::EditState::Approved(_))
		) {
			return None;
		}
		update.as_ref().filter(|_| update.has_changed()).map(handle)
	};
	*receiver = early;
	update
}

impl Overlay {
	pub(crate) fn new(
		ctx: &egui::Context,
		window: &winit::window::Window,
		standalone_wayland_allowed: bool,
	) -> Self {
		let wayland = window
			.display_handle()
			.is_ok_and(|handle| matches!(handle.as_raw(), RawDisplayHandle::Wayland(_)));
		let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|desktop| {
			desktop
				.split(':')
				.any(|name| name.eq_ignore_ascii_case("KDE"))
		});
		let native = (standalone_wayland_allowed && wayland && kde)
			.then(|| {
				let ctx = ctx.clone();
				layer::Overlay::start(move || ctx.request_repaint())
			})
			.flatten();
		Self {
			native,
			avatars: HashMap::new(),
			monitors: Vec::new(),
			toast: None,
			geometry_fallback: None,
			toast_serial: 0,
			foreground: None,
			foreground_attempted: false,
			catalog: None,
		}
	}

	pub(crate) fn game_gate(
		&mut self,
		enabled: bool,
		registered: &[model::registered_games::RegisteredGame],
		runtime: &tokio::runtime::Runtime,
		ctx: &egui::Context,
		now: Instant,
	) -> GameGate {
		if !enabled || self.native.is_none() {
			self.foreground = None;
			self.foreground_attempted = false;
			return GameGate::default();
		}
		if !self.foreground_attempted {
			self.foreground_attempted = true;
			let wake = ctx.clone();
			self.foreground = ForegroundWindow::start(move || wake.request_repaint());
		}
		let Some(window) = &self.foreground else {
			return GameGate::default();
		};
		let catalog = self.catalog.get_or_insert_with(|| {
			let catalog = Arc::new(OnceLock::new());
			let result = Arc::clone(&catalog);
			let ctx = ctx.clone();
			runtime.spawn_blocking(move || {
				let _ = result.set(crate::game_activity::local_game_index());
				ctx.request_repaint();
			});
			catalog
		});
		window
			.current(now)
			.filter(|evidence| {
				evidence.executable().to_str().is_some_and(|path| {
					crate::game_activity::recognizes_foreground_executable(
						catalog.get(),
						registered,
						path,
					)
				})
			})
			.map_or_else(GameGate::default, |evidence| {
				GameGate::recognized(evidence.generation(), evidence.valid_until())
			})
	}

	pub(crate) fn receive(&mut self, ui: &mut ui::MessagingUi, settings_visible: bool) {
		self.receive_updates(ui, settings_visible, false);
	}

	pub(crate) fn finish_frame(
		&mut self,
		ui: &mut ui::MessagingUi,
		settings_visible: bool,
	) -> bool {
		self.receive_updates(ui, settings_visible, true)
	}

	fn receive_updates(
		&mut self,
		ui: &mut ui::MessagingUi,
		settings_visible: bool,
		finish_frame: bool,
	) -> bool {
		let page_open = settings_visible && ui.overlay_page_open();
		let settings = &mut ui.voice_overlay;
		let mut changed = false;
		if !page_open {
			changed = settings.placement().is_some();
			settings.cancel();
		}
		let Some(native) = &mut self.native else {
			settings.set_displays(Availability::Unavailable, Vec::new());
			return changed;
		};
		let status = native.status.borrow_and_update();
		if status.has_changed() {
			let (availability, monitors): (_, &[_]) = match &*status {
				layer::Status::Starting => (Availability::Starting, &[]),
				layer::Status::Unavailable => (Availability::Unavailable, &[]),
				layer::Status::Available {
					monitors,
					editing,
					fonts,
				} => {
					settings.fonts.clone_from(fonts);
					(
						if *editing {
							Availability::Available
						} else {
							Availability::EditingUnavailable
						},
						monitors,
					)
				}
			};
			self.monitors = monitors.to_vec();
			settings.set_displays(
				availability,
				monitors
					.iter()
					.map(|monitor| Display {
						name: monitor.name.clone(),
						label: monitor.label.clone(),
						id: monitor.id,
						logical_size: monitor.logical_size,
						live_supported: monitor.live_supported,
						notifications_supported: monitor.notifications_supported,
						layout_supported: monitor.can_edit_layout(),
					})
					.collect(),
			);
		}
		// Keep the latest topology and edit snapshots stable through validation and apply.
		changed |= receive_edit(&mut native.edit, finish_frame, |edit| match edit.state {
			layer::EditState::Layout(ack) => {
				settings.report_layout(edit.generation, &edit.monitor, ack);
				false
			}
			layer::EditState::Approved(ack) => settings.apply_layout(
				edit.generation,
				&edit.monitor,
				ack,
				&mut ui.overlay_notifications.saved,
			),
			layer::EditState::Closed => {
				settings.report_closed(edit.generation, &edit.monitor);
				false
			}
		})
		.unwrap_or_default();
		drop(status);
		if finish_frame {
			changed |= settings.save_if_requested();
		}
		let rejected = *native.rejected_toast.borrow_and_update();
		changed |= rejected.is_some_and(|rejected| self.reject_toast(rejected));
		if self.toast.is_some() && settings.placement().is_some() {
			self.clear_toast();
			changed = true;
		}
		changed
	}

	fn reject_toast(&mut self, rejected: layer::ToastRejection) -> bool {
		if self
			.toast
			.as_ref()
			.is_none_or(|slot| slot.toast.serial != rejected.serial)
		{
			return false;
		}
		if rejected.fallback {
			self.geometry_fallback = self.toast.take();
		}
		self.clear_toast();
		true
	}

	pub(crate) fn take_geometry_fallback(
		&mut self,
		state: &State,
		ui: &ui::MessagingUi,
		(focused, fixture, game): (bool, bool, GameGate),
		now: Instant,
	) -> Option<DesktopAlert> {
		let slot = self.geometry_fallback.take()?;
		let lease = game.active(now)?;
		if lease.generation == slot.toast.foreground_generation
			&& slot.toast.active_at(now)
			&& ui.notifications_enabled
			&& slot.live.still_eligible(state, ui, focused, fixture)
		{
			slot.fallback
		} else {
			None
		}
	}

	fn clear_toast(&mut self) {
		self.toast = None;
		if let Some(native) = &self.native {
			native.clear_toast();
		}
	}

	pub(crate) fn dismiss_notifications(&mut self) {
		self.geometry_fallback = None;
		if self.toast.is_some() {
			self.clear_toast();
		}
	}

	pub(crate) fn disable(&mut self) {
		if let Some(native) = &self.native {
			native.update(layer::Frame::default());
		}
		self.toast = None;
		self.geometry_fallback = None;
		self.foreground = None;
		self.foreground_attempted = false;
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

	pub(crate) fn prepare_roster(
		&mut self,
		state: &State,
		ui: &mut ui::MessagingUi,
		voice: &crate::voice::Voice,
		worker: Option<&crate::avatars::AvatarWorker>,
	) -> layer::Frame {
		ui.voice_overlay.paused_after_move = false;
		if self.native.is_none() {
			return layer::Frame::default();
		}
		let settings = &ui.voice_overlay;
		let editor = settings.placement().map(|placement| layer::Editor {
			generation: placement.generation,
			monitor: placement.monitor.clone(),
			positions: placement.initial,
			corner: placement.corner,
			command: placement.command,
			mode: match placement.mode {
				Mode::Identify(until) => layer::Mode::Identify { until },
				Mode::Preview => layer::Mode::Preview {
					handle: ui.language.text("voice-overlay-drag-handle"),
					notification_handle: ui.language.text("overlay-notifications-drag-handle"),
				},
			},
		});
		let position = settings.saved.position;
		let mode = settings.saved.roster;
		let appearance = settings.saved.appearance.clone();
		let preview_appearance = settings.preview_appearance().cloned();
		let selected = settings
			.saved
			.monitor
			.as_deref()
			.filter(|_| settings.enabled)
			.and_then(|name| {
				self.monitors
					.iter()
					.find(|monitor| monitor.name == name && monitor.live_supported)
			});
		let slots = selected.map_or(0, |monitor| layer::roster_slots(monitor.logical_size));
		let monitor = selected.map(|monitor| monitor.name.clone());
		let source = if monitor.is_some() && !state.demo {
			voice.overlay_source(state)
		} else {
			None
		};
		ui.voice_overlay.paused_after_move = matches!(source, Some(Err(_)));
		let mut rows = Vec::new();
		let mut omitted = 0;
		if monitor.is_some() {
			if state.demo {
				rows.push((
					"Demo speaker".into(),
					state
						.user
						.as_ref()
						.filter(|_| appearance.show_avatars)
						.map(model::User::avatar_key),
					None,
				));
			} else if let Some(Ok((_, speakers))) = source {
				let (participants, overflow) = ui.active_voice_speakers(
					state,
					speakers.as_ref().map_or(&[][..], |ids| ids.as_slice()),
					mode,
					slots,
				);
				omitted = overflow;
				for row in participants {
					let name = label(row.name, 80, 256);
					let name = if name.is_empty() {
						ui.language.text("voice-overlay-participant")
					} else {
						name
					};
					let avatar = row
						.user
						.filter(|_| appearance.show_avatars)
						.map(model::User::avatar_key);
					let status = (mode == RosterMode::Everyone).then_some(layer::SpeakerStatus {
						speaking: row.speaking,
						muted: row.shown.muted || row.shown.server_muted,
						deafened: row.shown.deafened || row.shown.server_deafened,
						locally_muted: row.locally_muted,
					});
					rows.push((name, avatar, status));
				}
			}
		}
		// Retain recent avatars through brief silences, without growing beyond two full overlays.
		if monitor.is_none() || !appearance.show_avatars {
			self.avatars.clear();
		} else if self.avatars.len() > MAX_ROWS {
			self.avatars.retain(|key, _| {
				rows.iter()
					.any(|(_, active, _)| active.as_ref() == Some(key))
			});
		}
		let mut rows: Vec<_> = rows
			.into_iter()
			.map(|(name, key, status)| {
				let avatar = key.and_then(|key| {
					let avatar = self.avatars.entry(key.clone()).or_default();
					if !avatar.requested
						&& let Some(worker) = worker
					{
						avatar.requested = worker.request(key);
					}
					avatar.pixels.clone()
				});
				layer::Speaker {
					name,
					avatar,
					status,
					summary: false,
				}
			})
			.collect();
		if omitted > 0 {
			rows.push(layer::Speaker {
				name: ui::i18n::translate_count("voice-overlay-more", omitted, &[]),
				avatar: None,
				status: None,
				summary: true,
			});
		}
		layer::Frame {
			monitor,
			rows,
			row_slots: slots,
			position,
			editor,
			appearance,
			preview_appearance,
			toast: None,
		}
	}

	fn toast_output(&self, ui: &ui::MessagingUi, frame: &layer::Frame) -> Option<u32> {
		if frame.editor.is_some() {
			return None;
		}
		let monitor = self
			.monitors
			.iter()
			.find(|monitor| Some(&monitor.name) == ui.voice_overlay.saved.monitor.as_ref())?;
		let voice = (!frame.rows.is_empty() && frame.monitor.as_ref() == Some(&monitor.name))
			.then_some(frame.position);
		let settings = ui.overlay_notifications.saved;
		monitor
			.can_show_toast(settings.corner, settings.position, voice)
			.then_some(monitor.id)
	}

	pub(crate) fn refresh_toast(
		&mut self,
		state: &State,
		ui: &ui::MessagingUi,
		frame: &layer::Frame,
		(focused, fixture, game): (bool, bool, GameGate),
		now: Instant,
	) {
		let settings = ui.overlay_notifications.saved;
		let game = game.active(now);
		let output = self.toast_output(ui, frame);
		if self.toast.as_ref().is_some_and(|slot| {
			game.is_none_or(|lease| lease.generation != slot.toast.foreground_generation)
				|| !settings.enabled
				|| !slot.current(now, output, settings)
				|| !slot.live.still_eligible(state, ui, focused, fixture)
		}) {
			self.clear_toast();
		}
		if let (Some(slot), Some(lease)) = (&mut self.toast, game)
			&& slot.toast.foreground_until != lease.until
		{
			Arc::make_mut(&mut slot.toast).foreground_until = lease.until;
		}
	}

	pub(crate) fn admit_notification(
		&mut self,
		state: &State,
		ui: &ui::MessagingUi,
		frame: &layer::Frame,
		candidate: &NotificationCandidate,
		(game, now): (GameGate, Instant),
	) -> bool {
		let Some(lease) = game.active(now) else {
			return false;
		};
		let settings = ui.overlay_notifications.saved;
		if !settings.enabled {
			return false;
		}
		let Some(output) = self.toast_output(ui, frame) else {
			return false;
		};
		let Some(native) = &self.native else {
			return false;
		};
		self.toast_serial = self.toast_serial.wrapping_add(1);
		let slot =
			ToastSlot::notification(state, ui, candidate, self.toast_serial, output, now, lease);
		let mut frame = frame.clone();
		frame.toast = Some(slot.toast.clone());
		if !native.submit_toast(frame) {
			return false;
		}
		self.toast = Some(slot);
		true
	}

	pub(crate) fn publish(&self, mut frame: layer::Frame, ctx: &egui::Context) {
		if let Some(native) = &self.native {
			if let Some(slot) = &self.toast {
				ctx.request_repaint_after(
					slot.toast
						.until
						.min(slot.toast.foreground_until)
						.saturating_duration_since(Instant::now())
						.min(Duration::from_millis(100)),
				);
				frame.toast = Some(slot.toast.clone());
			}
			native.update(frame);
		}
	}
}

#[test]
fn layout_approval_stays_unread_until_ui_finishes_and_native_closure_can_replace_it() {
	use model::voice_overlay::{LayoutAck, LayoutPositions, LayoutStatus};

	let (sender, mut receiver) = watch::channel(None);
	let edit = |state| layer::EditUpdate {
		generation: 1,
		monitor: "DP-1".into(),
		state,
	};
	let ack = LayoutAck {
		revision: 1,
		command: 1,
		positions: LayoutPositions::default(),
		dragging: false,
		status: LayoutStatus::Ready,
	};
	let state = |edit: &layer::EditUpdate| edit.state;
	sender.send_replace(Some(edit(layer::EditState::Layout(ack))));
	assert!(matches!(
		receive_edit(&mut receiver, false, state),
		Some(layer::EditState::Layout(_))
	));
	assert!(receive_edit(&mut receiver, true, state).is_none());

	sender.send_replace(Some(edit(layer::EditState::Approved(ack))));
	assert!(receive_edit(&mut receiver, false, state).is_none());
	assert!(receiver.has_changed().unwrap());
	assert!(matches!(
		receive_edit(&mut receiver, true, state),
		Some(layer::EditState::Approved(_))
	));
	assert!(receive_edit(&mut receiver, true, state).is_none());

	sender.send_replace(Some(edit(layer::EditState::Approved(ack))));
	assert!(receive_edit(&mut receiver, false, state).is_none());
	sender.send_replace(Some(edit(layer::EditState::Closed)));
	assert!(matches!(
		receive_edit(&mut receiver, true, state),
		Some(layer::EditState::Closed)
	));
}

#[test]
fn only_visible_active_speakers_are_listed_with_their_server_nicknames() {
	let mut state = test_support::voice_demo_state();
	state.voice.roster[0].member.as_mut().unwrap().nick = Some("Server nickname".into());
	let ui = ui::MessagingUi::default();
	// A muted member in the same call, a member of another call, and an unknown user.
	let sample = [Id(2), Id(1), Id(4), Id(999)];
	let (rows, _) = ui.active_voice_speakers(&state, &sample, RosterMode::SpeakingOnly, 8);
	assert_eq!(rows.len(), 1);
	assert_eq!(
		(rows[0].shown.user, rows[0].name, rows[0].user.unwrap().id),
		(Id(1), "Server nickname", Id(1))
	);
	let (rows, omitted) = ui.active_voice_speakers(&state, &[], RosterMode::Everyone, 8);
	assert_eq!(omitted, 0);
	assert!(
		rows.iter()
			.any(|row| row.shown.user == Id(2) && row.shown.muted)
	);
	assert!(
		rows.iter()
			.all(|row| !row.speaking && row.shown.user != Id(4))
	);
}

#[test]
fn overlay_toast_is_metadata_first_and_expires_or_invalidates_without_replay() {
	use client_core::{Envelope, Event, notifications};
	let mut state = test_support::demo_state();
	let dm = state.channels.iter().find(|c| c.kind == 1).unwrap().clone();
	state
		.apply_notification_preferences(notifications::Event::Presence(Some(false)))
		.unwrap();
	state
		.apply_notification_preferences(notifications::Event::Settings {
			entries: vec![notifications::Setting {
				muted: Some(false),
				level: Some(0),
				..Default::default()
			}],
			replace: true,
		})
		.unwrap();
	let mut message = test_support::message(9_000_000_000, dm.id);
	message.content = "A fictional excerpt".into();
	message.author = dm.recipients[0].clone();
	message.author.name = format!("\u{202e}{}", "𐐷".repeat(80));
	state.set_notification_excerpts(true);
	state.apply(Envelope {
		generation: state.generation,
		event: Event::Message(message),
	});
	let candidate = state.take_notification_candidate().unwrap();
	state.demo = false;
	state.auth = AuthState::Authenticated;
	let mut ui = ui::MessagingUi::default();
	ui.overlay_notifications.saved.enabled = true;
	let now = Instant::now();
	let lease = now + Duration::from_secs(2);
	let game = GameGate::recognized(1, lease);
	let evidence = game.active(now).unwrap();
	let metadata = ToastSlot::notification(&state, &ui, &candidate, 1, 42, now, evidence);
	assert_eq!(metadata.toast.excerpt, None);
	assert!(!metadata.toast.sender.contains('\u{202e}'));
	assert!(metadata.toast.sender.len() <= 256 && !metadata.toast.channel.is_empty());
	let mut overlay = Overlay {
		native: None,
		avatars: HashMap::new(),
		monitors: vec![layer::Monitor {
			id: 42,
			name: "DP-2".into(),
			label: "Synthetic".into(),
			logical_size: (960, 720),
			live_supported: true,
			notifications_supported: true,
		}],
		toast: Some(metadata),
		geometry_fallback: None,
		toast_serial: 1,
		foreground: None,
		foreground_attempted: false,
		catalog: None,
	};
	ui.voice_overlay.saved.monitor = Some("DP-2".into());
	ui.overlay_notifications.saved.include_excerpts = true;
	let frame = layer::Frame::default();
	overlay.refresh_toast(&state, &ui, &frame, (false, false, game), now);
	assert!(
		overlay.toast.is_none(),
		"changing the opt-in clears the previous alert"
	);
	let included = ToastSlot::notification(&state, &ui, &candidate, 2, 42, now, evidence);
	assert_eq!(
		included.toast.excerpt.as_deref(),
		Some("A fictional excerpt")
	);
	assert!(included.current(now, Some(42), ui.overlay_notifications.saved));
	assert!(
		!included.current(now, Some(43), ui.overlay_notifications.saved),
		"replug is a different output"
	);
	overlay.toast = Some(included);
	overlay.refresh_toast(&state, &ui, &frame, (false, false, game), now);
	assert!(
		overlay.toast.is_some(),
		"a verified foreground game keeps the toast"
	);
	overlay.refresh_toast(
		&state,
		&ui,
		&frame,
		(false, false, GameGate::default()),
		now,
	);
	assert!(
		overlay.toast.is_none(),
		"focus loss clears rather than replays the toast"
	);
	overlay.refresh_toast(&state, &ui, &frame, (false, false, game), now);
	assert!(
		overlay.toast.is_none(),
		"returning to the game never replays the alert"
	);
	overlay.toast = Some(ToastSlot::notification(
		&state, &ui, &candidate, 3, 42, now, evidence,
	));
	let regained = GameGate::recognized(2, lease);
	overlay.refresh_toast(&state, &ui, &frame, (false, false, regained), now);
	assert!(
		overlay.toast.is_none(),
		"coalesced focus loss still invalidates the old toast"
	);
	let mut desktop = 0;
	for os_enabled in [false, true] {
		assert_eq!(
			crate::notification_runtime::route_visual(
				&candidate,
				os_enabled,
				|candidate| overlay.admit_notification(
					&state,
					&ui,
					&frame,
					candidate,
					(game, lease)
				),
				|_| desktop += 1,
			),
			os_enabled
		);
	}
	assert_eq!(
		desktop, 1,
		"new alerts at the expiry boundary honor the desktop fallback preference"
	);
	ui.notifications_enabled = true;
	overlay.toast = Some(ToastSlot::notification(
		&state, &ui, &candidate, 4, 42, now, evidence,
	));
	assert!(overlay.reject_toast(layer::ToastRejection {
		serial: 4,
		fallback: true
	}));
	assert!(
		overlay
			.take_geometry_fallback(&state, &ui, (false, false, game), now)
			.is_some()
	);
	assert!(
		overlay
			.take_geometry_fallback(&state, &ui, (false, false, game), now)
			.is_none()
	);
	overlay.toast = Some(ToastSlot::notification(
		&state, &ui, &candidate, 5, 42, now, evidence,
	));
	assert!(overlay.reject_toast(layer::ToastRejection {
		serial: 5,
		fallback: false
	}));
	assert!(
		overlay
			.take_geometry_fallback(&state, &ui, (false, false, game), now)
			.is_none()
	);
}
