use super::{Shell, geometry};
use model::voice_overlay::{
	Appearance, LayoutAck, LayoutAction, LayoutCommand, LayoutPanel, LayoutPositions, LayoutStatus,
	Position, ToastCorner,
};
use smithay_client_toolkit::{
	compositor::Region,
	output::OutputInfo,
	seat::{
		Capability, SeatHandler, SeatState,
		pointer::{PointerEvent, PointerEventKind, PointerHandler},
	},
	shell::{
		WaylandSurface,
		wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
	},
};
use std::time::Instant;
use wayland_client::{
	Connection, Proxy, QueueHandle,
	protocol::{wl_output, wl_pointer, wl_seat, wl_subsurface, wl_surface},
};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;

const BORDER: i32 = 2;
pub(super) const HANDLE_HEIGHT: i32 = 26;
const PRIMARY: u32 = 0x110;
const NUDGE: i32 = 8;

#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
	Identify {
		until: Instant,
	},
	Preview {
		handle: String,
		notification_handle: String,
	},
}

#[derive(Clone, PartialEq, Eq)]
pub struct Editor {
	pub generation: u64,
	pub monitor: String,
	/// Initial positions for this generation; the native draft owns subsequent moves.
	pub positions: LayoutPositions,
	pub corner: ToastCorner,
	pub command: Option<LayoutCommand>,
	pub mode: Mode,
}

impl Editor {
	pub(super) fn same_layout(&self, other: &Self) -> bool {
		self.generation == other.generation
			&& self.monitor == other.monitor
			&& self.positions == other.positions
			&& self.corner == other.corner
			&& self.mode == other.mode
	}
}

#[derive(Clone, PartialEq, Eq)]
pub struct EditUpdate {
	pub generation: u64,
	pub monitor: String,
	pub state: EditState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditState {
	Layout(LayoutAck),
	Approved(LayoutAck),
	Closed,
}

struct Child {
	viewport: WpViewport,
	role: wl_subsurface::WlSubsurface,
	surface: wl_surface::WlSurface,
}

impl Child {
	fn new(
		shell: &Shell,
		qh: &QueueHandle<Shell>,
		parent: &wl_surface::WlSurface,
	) -> Result<Self, ()> {
		let (role, surface) = shell
			.subcompositor
			.as_ref()
			.ok_or(())?
			.create_subsurface(parent.clone(), qh);
		let viewport = shell
			.viewporter
			.as_ref()
			.ok_or(())?
			.get_viewport(&surface, qh, ());
		surface.set_input_region(Some(
			Region::new(&shell.compositor).map_err(|_| ())?.wl_region(),
		));
		Ok(Self {
			role,
			surface,
			viewport,
		})
	}

	fn attach_preview(
		&self,
		buffer: &super::Buffer,
		(width, height): (i32, i32),
	) -> Result<(), ()> {
		let scale = super::SCALE;
		self.surface.set_buffer_scale(scale);
		self.surface
			.damage_buffer(0, 0, width * scale, height * scale);
		buffer.attach_to(&self.surface).map_err(|_| ())?;
		self.surface.commit();
		Ok(())
	}
}

impl Drop for Child {
	fn drop(&mut self) {
		self.viewport.destroy();
		self.role.destroy();
		self.surface.destroy();
	}
}

struct Preview {
	notification: Child,
	handles: [Child; 2],
}

struct Handle {
	panel: LayoutPanel,
	origin: (i32, i32),
	panel_origin: (i32, i32),
}

impl Handle {
	fn width(&self) -> i32 {
		geometry::footprint(self.panel).0
	}

	fn hit(&self, x: f64, y: f64) -> Option<(LayoutPanel, (f64, f64))> {
		((0. ..f64::from(self.width())).contains(&(x - f64::from(self.origin.0)))
			&& (0. ..f64::from(HANDLE_HEIGHT)).contains(&(y - f64::from(self.origin.1))))
		.then_some((
			self.panel,
			(
				x - f64::from(self.panel_origin.0),
				y - f64::from(self.panel_origin.1),
			),
		))
	}
}

fn panel_handles(voice: (i32, i32), notification: (i32, i32)) -> [Handle; 2] {
	let voice = Handle {
		panel: LayoutPanel::Voice,
		origin: voice,
		panel_origin: voice,
	};
	let mut notification = Handle {
		panel: LayoutPanel::Notification,
		origin: notification,
		panel_origin: notification,
	};
	if voice.origin.0 < notification.origin.0 + notification.width()
		&& notification.origin.0 < voice.origin.0 + voice.width()
		&& voice.origin.1.abs_diff(notification.origin.1) < HANDLE_HEIGHT as u32
	{
		// This second slot clears either overlapping top edge and stays inside the toast footprint.
		notification.origin.1 += 2 * HANDLE_HEIGHT;
	}
	[voice, notification]
}

fn matches_configure(expected: (i32, i32), (width, height): (u32, u32)) -> bool {
	// A zero axis leaves that dimension at the selected output's logical size.
	(width == 0 || width == expected.0 as u32) && (height == 0 || height == expected.1 as u32)
}

struct Draft {
	positions: LayoutPositions,
	revision: u64,
	command: u64,
	approved: bool,
}

impl Draft {
	fn new(positions: LayoutPositions) -> Self {
		Self {
			positions,
			revision: 1,
			command: 0,
			approved: false,
		}
	}

	fn resolve_legacy_notification(&mut self, size: (i32, i32), corner: ToastCorner) {
		if self.positions.notification.is_none() {
			self.positions.notification = geometry::editor_notification_position(size, corner);
		}
	}

	fn position(&self, panel: LayoutPanel) -> Option<Position> {
		match panel {
			LayoutPanel::Voice => self.positions.voice,
			LayoutPanel::Notification => self.positions.notification,
		}
	}

	fn set(&mut self, panel: LayoutPanel, value: Option<Position>) {
		match panel {
			LayoutPanel::Voice => self.positions.voice = value,
			LayoutPanel::Notification => self.positions.notification = value,
		}
	}

	fn origin(
		panel: LayoutPanel,
		size: (i32, i32),
		corner: ToastCorner,
		position: Option<Position>,
	) -> Option<(i32, i32)> {
		match panel {
			LayoutPanel::Voice => Some(geometry::offset(size, position)),
			LayoutPanel::Notification => geometry::notification_offset(size, corner, position),
		}
	}

	fn move_panel(
		&mut self,
		panel: LayoutPanel,
		size: (i32, i32),
		corner: ToastCorner,
		wanted: (i32, i32),
	) {
		let previous = self.position(panel);
		let next = geometry::drag(size, panel, previous, wanted);
		if Self::origin(panel, size, corner, previous) != Self::origin(panel, size, corner, next) {
			self.set(panel, next);
		}
	}

	fn state(&self, size: (i32, i32), corner: ToastCorner, dragging: bool) -> EditState {
		let ack = LayoutAck {
			revision: self.revision,
			command: self.command,
			positions: self.positions,
			dragging,
			status: geometry::layout_status(size, corner, self.positions),
		};
		if self.approved {
			EditState::Approved(ack)
		} else {
			EditState::Layout(ack)
		}
	}

	fn apply(
		&mut self,
		size: (i32, i32),
		corner: ToastCorner,
		dragging: bool,
		command: LayoutCommand,
	) -> bool {
		if command.sequence <= self.command {
			return false;
		}
		if self.approved
			&& !matches!(command.action, LayoutAction::Save { revision } if revision == self.revision)
		{
			return false;
		}
		self.command = command.sequence;
		if dragging {
			return true;
		}
		let before = self.positions;
		match command.action {
			LayoutAction::Save { revision }
				if revision == self.revision
					&& geometry::layout_status(size, corner, self.positions)
						== LayoutStatus::Ready =>
			{
				self.approved = true;
			}
			LayoutAction::Reset(panel) if !self.approved => self.set(
				panel,
				match panel {
					LayoutPanel::Voice => None,
					LayoutPanel::Notification => {
						geometry::editor_notification_position(size, ToastCorner::default())
					}
				},
			),
			LayoutAction::Nudge { panel, x, y } if !self.approved && geometry::can_edit(size) => {
				if let Some((left, top)) = Self::origin(panel, size, corner, self.position(panel)) {
					self.move_panel(
						panel,
						size,
						corner,
						(
							left.saturating_add(i32::from(x) * NUDGE),
							top.saturating_add(i32::from(y) * NUDGE),
						),
					);
				}
			}
			_ => {}
		}
		if self.positions != before {
			self.revision += 1;
		}
		true
	}
}

struct Grab {
	pointer: wl_pointer::WlPointer,
	panel: LayoutPanel,
	delta: (f64, f64),
	original: Option<Position>,
}

pub(super) struct Tree {
	viewport: WpViewport,
	borders: [Child; 4],
	voice: Child,
	preview: Option<Preview>,
	pub(super) layer: LayerSurface,
	pub(super) output: wl_output::WlOutput,
	pub(super) request: Editor,
	pub(super) expected_size: (i32, i32),
	output_transform: wl_output::Transform,
	output_scale: i32,
	output_position: Option<(i32, i32)>,
	label: String,
	configured: bool,
	draft: Draft,
	origins: ((i32, i32), Option<(i32, i32)>),
	grab: Option<Grab>,
	voice_dirty: bool,
	layout_dirty: bool,
	mapped: bool,
}

impl Tree {
	pub(super) fn new(
		shell: &Shell,
		qh: &QueueHandle<Shell>,
		output: wl_output::WlOutput,
		request: Editor,
		expected_size: (i32, i32),
		label: String,
	) -> Result<Self, ()> {
		let info = shell.outputs.info(&output).ok_or(())?;
		let surface = shell.compositor.create_surface(qh);
		surface.set_input_region(Some(
			Region::new(&shell.compositor).map_err(|_| ())?.wl_region(),
		));
		let layer = shell.shell.create_layer_surface(
			qh,
			surface,
			Layer::Overlay,
			Some("serein-speaker-placement"),
			Some(&output),
		);
		layer.set_anchor(Anchor::all());
		layer.set_exclusive_zone(-1);
		layer.set_keyboard_interactivity(KeyboardInteractivity::None);
		layer.set_size(0, 0);
		layer.commit();
		let surface = layer.wl_surface();
		let viewport = shell
			.viewporter
			.as_ref()
			.ok_or(())?
			.get_viewport(surface, qh, ());
		let borders = [
			Child::new(shell, qh, surface)?,
			Child::new(shell, qh, surface)?,
			Child::new(shell, qh, surface)?,
			Child::new(shell, qh, surface)?,
		];
		let voice = Child::new(shell, qh, surface)?;
		let preview = if matches!(request.mode, Mode::Preview { .. }) {
			Some(Preview {
				notification: Child::new(shell, qh, surface)?,
				// New siblings are stacked above earlier children, so neither body can hide a handle.
				handles: [
					Child::new(shell, qh, surface)?,
					Child::new(shell, qh, surface)?,
				],
			})
		} else {
			None
		};
		// These immutable pixels need no release tracking, even when shared by the four borders.
		for border in &borders {
			border.surface.attach(Some(shell.red.wl_buffer()), 0, 0);
			border.surface.damage_buffer(0, 0, 1, 1);
		}
		let draft = Draft::new(request.positions);
		Ok(Self {
			viewport,
			borders,
			voice,
			preview,
			layer,
			output,
			request,
			expected_size,
			output_transform: info.transform,
			output_scale: info.scale_factor,
			output_position: info.logical_position,
			label,
			configured: false,
			draft,
			origins: ((0, 0), None),
			grab: None,
			voice_dirty: true,
			layout_dirty: true,
			mapped: false,
		})
	}

	pub(super) fn matches_geometry(&self, info: &OutputInfo) -> bool {
		info.logical_size == Some(self.expected_size)
			&& info.transform == self.output_transform
			&& info.scale_factor == self.output_scale
			&& info.logical_position == self.output_position
	}

	pub(super) fn update(&self, state: EditState) -> EditUpdate {
		EditUpdate {
			generation: self.request.generation,
			monitor: self.request.monitor.clone(),
			state,
		}
	}

	fn layout(&self) -> EditUpdate {
		self.update(
			self.draft
				.state(self.expected_size, self.request.corner, self.grab.is_some()),
		)
	}

	pub(super) fn configure(&mut self, size: (u32, u32)) -> Result<Option<EditUpdate>, ()> {
		if !matches_configure(self.expected_size, size) {
			return Err(());
		}
		let first = !std::mem::replace(&mut self.configured, true);
		if first && self.preview.is_some() {
			self.draft
				.resolve_legacy_notification(self.expected_size, self.request.corner);
			return Ok(Some(self.layout()));
		}
		Ok(None)
	}

	pub(super) fn apply(&mut self, command: LayoutCommand) -> Option<EditUpdate> {
		if !self.configured || self.preview.is_none() {
			return None;
		}
		let approved = self.draft.approved;
		if !self.draft.apply(
			self.expected_size,
			self.request.corner,
			self.grab.is_some(),
			command,
		) {
			return None;
		}
		self.layout_dirty |= approved != self.draft.approved;
		Some(self.layout())
	}

	fn cancel_grab(&mut self) -> bool {
		let Some(grab) = self.grab.take() else {
			return false;
		};
		self.draft.set(grab.panel, grab.original);
		self.draft.revision += 1;
		true
	}

	pub(super) fn repaint(&mut self) {
		self.voice_dirty = true;
	}

	pub(super) fn paint(shell: &mut Shell, appearance: &Appearance) -> Result<(), ()> {
		let Some(tree) = shell.tree.as_mut().filter(|tree| tree.configured) else {
			return Ok(());
		};
		let size = tree.expected_size;
		let (width, height) = size;
		let extent = geometry::extent(size);
		let editable = tree.preview.is_some()
			&& geometry::layout_status(size, tree.request.corner, tree.draft.positions)
				!= LayoutStatus::TooSmall;
		let rows = if editable {
			&shell.preview_rows[..]
		} else {
			&[]
		};
		let new_preview = editable && !tree.mapped;
		if new_preview
			&& (shell.pool.canvas(&shell.preview_toast_buffer).is_none()
				|| shell
					.preview_handle_buffers
					.iter()
					.any(|buffer| shell.pool.canvas(buffer).is_none()))
		{
			return Ok(());
		}
		let mut pixels_changed = false;
		if tree.voice_dirty {
			if let Some(buffer) = shell
				.buffers
				.iter()
				.find(|&b| shell.pool.canvas(b).is_some())
			{
				let pixels = super::render(
					rows,
					Some(super::Decoration {
						label: &tree.label,
						preview: editable,
					}),
					appearance,
					super::SCALE,
					super::WIDTH,
				)?;
				shell
					.pool
					.canvas(buffer)
					.ok_or(())?
					.copy_from_slice(&pixels);
				super::attach_rows(buffer, &tree.voice.surface)?;
				tree.voice_dirty = false;
				pixels_changed = true;
			} else if !tree.mapped {
				return Ok(());
			}
		}
		if new_preview
			&& let Mode::Preview {
				handle,
				notification_handle,
			} = &tree.request.mode
		{
			let preview = tree.preview.as_ref().ok_or(())?;
			let buffer = &shell.preview_toast_buffer;
			shell
				.pool
				.canvas(buffer)
				.ok_or(())?
				.copy_from_slice(&super::toast::render_preview()?);
			preview
				.notification
				.attach_preview(buffer, super::toast::SIZE)?;
			for ((child, buffer), (label, width)) in preview
				.handles
				.iter()
				.zip(&shell.preview_handle_buffers)
				.zip([
					(handle, geometry::FOOTPRINT.0),
					(notification_handle, super::toast::SIZE.0),
				]) {
				shell
					.pool
					.canvas(buffer)
					.ok_or(())?
					.copy_from_slice(&super::render_preview_handle(label, width)?);
				child.attach_preview(buffer, (width, HANDLE_HEIGHT))?;
			}
		}
		let origins = if editable {
			(
				geometry::offset(size, tree.draft.positions.voice),
				geometry::notification_offset(
					size,
					tree.request.corner,
					tree.draft.positions.notification,
				),
			)
		} else {
			(
				((width - extent.0) / 2, (height - HANDLE_HEIGHT).max(0) / 2),
				None,
			)
		};
		if !tree.mapped {
			let (bw, bh) = (BORDER.min(width), BORDER.min(height));
			for (border, (x, y, w, h)) in tree.borders.iter().zip([
				(0, 0, width, bh),
				(0, height - bh, width, bh),
				(0, 0, bw, height),
				(width - bw, 0, bw, height),
			]) {
				border.viewport.set_destination(w, h);
				border.role.set_position(x, y);
				border.surface.commit();
			}
			tree.viewport.set_destination(width, height);
			tree.layer
				.wl_surface()
				.attach(Some(shell.transparent.wl_buffer()), 0, 0);
			tree.layer.wl_surface().damage_buffer(0, 0, 1, 1);
			super::crop(
				&tree.voice.viewport,
				(
					extent.0,
					if editable {
						extent.1
					} else {
						extent.1.min(HANDLE_HEIGHT)
					},
				),
			);
		}
		let move_content = !tree.mapped || tree.layout_dirty || origins != tree.origins;
		if move_content {
			let input = Region::new(&shell.compositor).map_err(|_| ())?;
			if let Some((notification, preview)) = origins.1.zip(tree.preview.as_ref()) {
				preview
					.notification
					.role
					.set_position(notification.0, notification.1);
				for (child, handle) in preview
					.handles
					.iter()
					.zip(panel_handles(origins.0, notification))
				{
					let (x, y) = handle.origin;
					child.role.set_position(x, y);
					if !tree.draft.approved {
						input.add(x, y, handle.width(), HANDLE_HEIGHT);
					}
				}
			}
			tree.layer
				.wl_surface()
				.set_input_region(Some(input.wl_region()));
			tree.voice.role.set_position(origins.0.0, origins.0.1);
			tree.origins = origins;
			tree.layout_dirty = false;
			tree.mapped = true;
		}
		if pixels_changed || move_content {
			tree.voice.surface.commit();
			tree.layer.commit();
		}
		Ok(())
	}

	fn hit_handle(&self, x: f64, y: f64) -> Option<(LayoutPanel, (f64, f64))> {
		panel_handles(self.origins.0, self.origins.1?)
			.iter()
			.find_map(|handle| handle.hit(x, y))
	}

	fn pointer(
		&mut self,
		pointer: &wl_pointer::WlPointer,
		events: &[PointerEvent],
	) -> Option<EditUpdate> {
		let size = self.expected_size;
		if !self.mapped || self.origins.1.is_none() || self.draft.approved {
			return None;
		}
		let before = (
			self.draft.positions,
			self.grab.is_some(),
			self.draft.revision,
		);
		for event in events {
			if &event.surface != self.layer.wl_surface() {
				continue;
			}
			let (x, y) = event.position;
			match event.kind {
				PointerEventKind::Press {
					button: PRIMARY, ..
				} if self.grab.is_none() => {
					if let Some((panel, delta)) = self.hit_handle(x, y) {
						self.grab = Some(Grab {
							pointer: pointer.clone(),
							panel,
							delta,
							original: self.draft.position(panel),
						});
					}
				}
				PointerEventKind::Motion { .. }
				| PointerEventKind::Release {
					button: PRIMARY, ..
				} => {
					if let Some(grab) = self.grab.as_ref().filter(|grab| &grab.pointer == pointer) {
						let wanted = (
							(x - grab.delta.0).round() as i32,
							(y - grab.delta.1).round() as i32,
						);
						self.draft
							.move_panel(grab.panel, size, self.request.corner, wanted);
						if matches!(event.kind, PointerEventKind::Release { .. }) {
							self.grab = None;
						}
					}
				}
				PointerEventKind::Leave { .. }
					if self
						.grab
						.as_ref()
						.is_some_and(|grab| &grab.pointer == pointer) =>
				{
					self.cancel_grab();
				}
				_ => {}
			}
		}
		if before.0 != self.draft.positions
			|| before.1 != self.grab.is_some()
			|| before.2 != self.draft.revision
		{
			if before.2 == self.draft.revision {
				self.draft.revision += 1;
			}
			return Some(self.layout());
		}
		None
	}
}

impl Drop for Tree {
	fn drop(&mut self) {
		self.viewport.destroy();
	}
}

impl PointerHandler for Shell {
	fn pointer_frame(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		pointer: &wl_pointer::WlPointer,
		events: &[PointerEvent],
	) {
		if let Some(update) = self
			.tree
			.as_mut()
			.and_then(|tree| tree.pointer(pointer, events))
		{
			self.edit_update = Some(update);
		}
	}
}

impl Shell {
	fn forget_pointer(&mut self, seat: &wl_seat::WlSeat) {
		if let Some(index) = self.pointers.iter().position(|(known, _)| known == seat) {
			let (_, pointer) = self.pointers.swap_remove(index);
			if let Some(tree) = &mut self.tree
				&& tree
					.grab
					.as_ref()
					.is_some_and(|grab| grab.pointer == pointer)
				&& tree.cancel_grab()
			{
				self.edit_update = Some(tree.layout());
			}
			if pointer.version() >= 3 {
				pointer.release();
			}
		}
	}
}

impl SeatHandler for Shell {
	fn seat_state(&mut self) -> &mut SeatState {
		&mut self.seats
	}
	fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
	fn new_capability(
		&mut self,
		_: &Connection,
		qh: &QueueHandle<Self>,
		seat: wl_seat::WlSeat,
		capability: Capability,
	) {
		if capability == Capability::Pointer
			&& self.pointers.len() < 8
			&& self.editing_available()
			&& let Ok(pointer) = self.seats.get_pointer(qh, &seat)
		{
			self.pointers.push((seat, pointer));
		}
	}
	fn remove_capability(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		seat: wl_seat::WlSeat,
		capability: Capability,
	) {
		if capability == Capability::Pointer {
			self.forget_pointer(&seat);
		}
	}
	fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
		self.forget_pointer(&seat);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fullscreen_configure_accepts_client_chosen_axes_but_not_conflicting_sizes() {
		let expected = (960, 720);
		assert!(matches_configure(expected, (0, 720)));
		assert!(matches_configure(expected, (960, 0)));
		assert!(!matches_configure(expected, (0, 719)));
		assert!(!matches_configure(expected, (959, 0)));
	}

	#[test]
	fn overlapping_preview_handles_are_reachable_without_capturing_body_clicks() {
		let origin = (0, 9);
		let handles = panel_handles(origin, origin);
		let hit = |x, y| handles.iter().find_map(|handle| handle.hit(x, y));
		assert_eq!(hit(10., 10.), Some((LayoutPanel::Voice, (10., 1.))));
		let lower = f64::from(handles[1].origin.1);
		assert!(handles[1].origin.1 + HANDLE_HEIGHT <= origin.1 + super::super::toast::SIZE.1);
		assert_eq!(
			hit(10., lower),
			Some((LayoutPanel::Notification, (10., lower - 9.)))
		);
		assert_eq!(
			hit(300., 10.),
			None,
			"the moved handle leaves its original slot click-through"
		);
		assert_eq!(hit(10., lower - 1.), None);

		// Even with no handle-to-handle overlap the toast body can cover the voice handle.
		let voice_y = origin.1 + HANDLE_HEIGHT;
		let handles = panel_handles((0, voice_y), origin);
		assert_eq!(
			handles[0].hit(10., f64::from(voice_y)),
			Some((LayoutPanel::Voice, (10., 0.)))
		);
		assert_eq!(handles[1].hit(10., f64::from(voice_y)), None);
		assert_eq!(handles[1].origin, origin);
	}

	#[test]
	fn unchanged_save_approves_the_normalized_legacy_position_from_the_first_ack() {
		let size = (960, 720);
		let corner = ToastCorner::BottomLeft;
		let mut draft = Draft::new(LayoutPositions::default());
		draft.resolve_legacy_notification(size, corner);
		let EditState::Layout(initial) = draft.state(size, corner, false) else {
			panic!("the initial draft must not be approved");
		};
		assert_eq!(initial.status, LayoutStatus::Ready);
		assert!(initial.positions.notification.is_some());
		assert_eq!(
			geometry::notification_offset(size, corner, initial.positions.notification),
			geometry::notification_offset(size, corner, None)
		);
		assert!(draft.apply(
			size,
			corner,
			false,
			LayoutCommand {
				sequence: 1,
				action: LayoutAction::Save {
					revision: initial.revision
				},
			}
		));
		assert_eq!(
			draft.state(size, corner, false),
			EditState::Approved(LayoutAck {
				command: 1,
				..initial
			})
		);
	}

	#[test]
	fn layout_commands_require_current_ready_ungrabbed_revision_and_lock_after_approval() {
		let size = (960, 720);
		let corner = ToastCorner::TopRight;
		let mut draft = Draft::new(LayoutPositions::default());
		draft.resolve_legacy_notification(size, corner);
		let initial = draft.revision;
		let command = |sequence, action| LayoutCommand { sequence, action };
		assert!(matches!(
			draft.state(size, corner, false),
			EditState::Layout(LayoutAck { command: 0, .. })
		));
		assert!(draft.apply(
			size,
			corner,
			false,
			command(
				1,
				LayoutAction::Nudge {
					panel: LayoutPanel::Notification,
					x: -1,
					y: 1
				}
			)
		));
		let moved = draft.positions;
		assert!(moved.notification.is_some());
		assert_eq!(moved.voice, None);
		assert!(draft.apply(
			size,
			corner,
			false,
			command(2, LayoutAction::Save { revision: initial })
		));
		assert!(!draft.approved);
		let revision = draft.revision;
		let save = LayoutAction::Save { revision };
		assert!(draft.apply(size, corner, true, command(3, save)));
		assert!(!draft.approved);
		assert!(!draft.apply(size, corner, false, command(3, save)));
		assert!(draft.apply(size, corner, false, command(4, save)));
		assert!(!draft.apply(
			size,
			corner,
			false,
			command(5, LayoutAction::Reset(LayoutPanel::Notification))
		));
		assert!(draft.apply(size, corner, false, command(6, save)));
		assert_eq!(
			draft.state(size, corner, false),
			EditState::Approved(LayoutAck {
				revision,
				command: 6,
				positions: moved,
				dragging: false,
				status: LayoutStatus::Ready,
			})
		);
	}

	#[test]
	fn independent_panel_moves_and_reset_can_create_overlap_but_cannot_be_saved() {
		let size = (960, 720);
		let corner = ToastCorner::TopLeft;
		let mut draft = Draft::new(LayoutPositions::default());
		draft.resolve_legacy_notification(size, corner);
		draft.move_panel(LayoutPanel::Voice, size, corner, (0, 0));
		let voice = draft.positions.voice;
		assert_eq!(voice, Some(Position { x: 0, y: 0 }));
		draft.move_panel(LayoutPanel::Notification, size, corner, (900, 850));
		let notification = draft.positions.notification;
		assert_eq!(
			notification,
			Some(Position {
				x: 10_000,
				y: 10_000
			})
		);
		assert_eq!(draft.positions.voice, voice);
		assert!(draft.apply(
			size,
			corner,
			false,
			LayoutCommand {
				sequence: 1,
				action: LayoutAction::Reset(LayoutPanel::Voice)
			}
		));
		assert_eq!(draft.positions.voice, None);
		assert_eq!(draft.positions.notification, notification);
		draft.move_panel(LayoutPanel::Voice, size, corner, (size.0, 0));
		let voice = draft.positions.voice;
		assert!(draft.apply(
			size,
			corner,
			false,
			LayoutCommand {
				sequence: 2,
				action: LayoutAction::Reset(LayoutPanel::Notification)
			}
		));
		assert_eq!(draft.positions.voice, voice);
		assert!(draft.positions.notification.is_some());
		assert_eq!(
			geometry::notification_offset(size, corner, draft.positions.notification),
			geometry::notification_offset(size, ToastCorner::default(), None)
		);
		let revision = draft.revision;
		let save = |sequence| LayoutCommand {
			sequence,
			action: LayoutAction::Save { revision },
		};
		assert!(draft.apply(size, corner, false, save(3)));
		assert!(matches!(
			draft.state(size, corner, false),
			EditState::Layout(LayoutAck {
				status: LayoutStatus::Overlap,
				..
			})
		));
		assert!(draft.apply((340, 300), corner, false, save(4)));
		assert!(matches!(
			draft.state((340, 300), corner, false),
			EditState::Layout(LayoutAck {
				status: LayoutStatus::TooSmall,
				..
			})
		));
	}
}
