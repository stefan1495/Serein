use super::{Shell, geometry};
use model::voice_overlay::{Appearance, Position};
use smithay_client_toolkit::{
	compositor::Region,
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

#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
	Identify { until: Instant },
	Preview { handle: String },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Editor {
	pub generation: u64,
	pub monitor: String,
	pub position: Option<Position>,
	pub mode: Mode,
}

#[derive(Clone, PartialEq, Eq)]
pub struct EditUpdate {
	pub generation: u64,
	pub monitor: String,
	pub state: EditState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EditState {
	Placement {
		position: Option<Position>,
		dragging: bool,
	},
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
}

impl Drop for Child {
	fn drop(&mut self) {
		self.viewport.destroy();
		self.role.destroy();
		self.surface.destroy();
	}
}

struct Grab {
	pointer: wl_pointer::WlPointer,
	delta: (f64, f64),
	original: Option<Position>,
}

pub(super) struct Tree {
	viewport: WpViewport,
	borders: [Child; 4],
	content: Child,
	pub(super) layer: LayerSurface,
	pub(super) output: wl_output::WlOutput,
	pub(super) request: Editor,
	label: String,
	size: Option<(i32, i32)>,
	position: Option<Position>,
	origin: (i32, i32),
	grab: Option<Grab>,
	dirty: bool,
	layout_dirty: bool,
	mapped: bool,
}

impl Tree {
	pub(super) fn new(
		shell: &Shell,
		qh: &QueueHandle<Shell>,
		output: wl_output::WlOutput,
		request: Editor,
		label: String,
	) -> Result<Self, ()> {
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
		let content = Child::new(shell, qh, surface)?;
		// These immutable pixels need no release tracking, even when shared by the four borders.
		for border in &borders {
			border.surface.attach(Some(shell.red.wl_buffer()), 0, 0);
			border.surface.damage_buffer(0, 0, 1, 1);
		}
		let position = request.position;
		Ok(Self {
			viewport,
			borders,
			content,
			layer,
			output,
			request,
			label,
			size: None,
			position,
			origin: (0, 0),
			grab: None,
			dirty: true,
			layout_dirty: true,
			mapped: false,
		})
	}

	pub(super) fn update(&self, state: EditState) -> EditUpdate {
		EditUpdate {
			generation: self.request.generation,
			monitor: self.request.monitor.clone(),
			state,
		}
	}

	fn placement(&self) -> EditUpdate {
		self.update(EditState::Placement {
			position: self.position,
			dragging: self.grab.is_some(),
		})
	}

	pub(super) fn configure(&mut self, size: (i32, i32)) -> Option<EditUpdate> {
		if self.size == Some(size) || size.0 <= 0 || size.1 <= 0 {
			return None;
		}
		let cancelled = self.cancel_grab();
		self.size = Some(size);
		self.layout_dirty = true;
		cancelled.then(|| self.placement())
	}

	fn cancel_grab(&mut self) -> bool {
		let Some(grab) = self.grab.take() else {
			return false;
		};
		self.position = grab.original;
		true
	}

	pub(super) fn repaint(&mut self) {
		self.dirty = true;
	}

	pub(super) fn paint(shell: &mut Shell, appearance: &Appearance) -> Result<(), ()> {
		let Some(tree) = &mut shell.tree else {
			return Ok(());
		};
		let Some((width, height)) = tree.size else {
			return Ok(());
		};
		let pixels_changed = tree.dirty;
		if pixels_changed {
			let Some(buffer) = shell
				.buffers
				.iter()
				.find(|&b| shell.pool.canvas(b).is_some())
			else {
				return Ok(());
			};
			let (rows, handle) = match &tree.request.mode {
				Mode::Identify { .. } => (&[][..], None),
				Mode::Preview { handle } => (&shell.preview_rows[..], Some(handle.as_str())),
			};
			let pixels = super::render(
				rows,
				Some(super::Decoration {
					label: &tree.label,
					handle,
				}),
				appearance,
				super::SCALE,
			)?;
			shell
				.pool
				.canvas(buffer)
				.ok_or(())?
				.copy_from_slice(&pixels);
			super::attach_rows(buffer, &tree.content.surface)?;
			tree.dirty = false;
		}
		let size = geometry::extent((width, height));
		let preview = matches!(tree.request.mode, Mode::Preview { .. });
		let origin = if preview {
			geometry::offset((width, height), tree.position)
		} else {
			((width - size.0) / 2, (height - HANDLE_HEIGHT).max(0) / 2)
		};
		if tree.layout_dirty {
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
			let content_height = if preview {
				size.1
			} else {
				size.1.min(HANDLE_HEIGHT)
			};
			super::crop(&tree.content.viewport, (size.0, content_height));
		}
		let move_content = !tree.mapped || tree.layout_dirty || origin != tree.origin;
		if move_content {
			let input = Region::new(&shell.compositor).map_err(|_| ())?;
			if preview {
				input.add(origin.0, origin.1, size.0, HANDLE_HEIGHT.min(size.1));
			}
			tree.layer
				.wl_surface()
				.set_input_region(Some(input.wl_region()));
			tree.content.role.set_position(origin.0, origin.1);
			tree.origin = origin;
			tree.layout_dirty = false;
			tree.mapped = true;
		}
		if pixels_changed || move_content {
			tree.content.surface.commit();
			tree.layer.commit();
		}
		Ok(())
	}

	fn pointer(
		&mut self,
		pointer: &wl_pointer::WlPointer,
		events: &[PointerEvent],
	) -> Option<EditUpdate> {
		let size = self.size?;
		if !self.mapped || !matches!(self.request.mode, Mode::Preview { .. }) {
			return None;
		}
		let before = (self.position, self.grab.is_some());
		for event in events {
			if &event.surface != self.layer.wl_surface() {
				continue;
			}
			let (x, y) = event.position;
			match event.kind {
				PointerEventKind::Press {
					button: PRIMARY, ..
				} if self.grab.is_none() => {
					let (dx, dy) = (x - f64::from(self.origin.0), y - f64::from(self.origin.1));
					let extent = geometry::extent(size);
					if dx >= 0.
						&& dx < f64::from(extent.0)
						&& dy >= 0. && dy < f64::from(HANDLE_HEIGHT.min(extent.1))
					{
						self.grab = Some(Grab {
							pointer: pointer.clone(),
							delta: (dx, dy),
							original: self.position,
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
						if wanted != geometry::offset(size, self.position) {
							self.position = geometry::drag(size, self.position, wanted).0;
						}
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
		(before != (self.position, self.grab.is_some())).then(|| self.placement())
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
				self.edit_update = Some(tree.placement());
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
