//! KDE Wayland speaker rows and temporary placement surfaces. No audio or IPC listeners.
mod geometry;
mod placement;

use gtk4::{cairo, pango, pango::prelude::*};
use model::voice_overlay::{Appearance, FontWeight, Position};
pub use placement::{EditState, EditUpdate, Editor, Mode};
use smithay_client_toolkit::{
	compositor::{CompositorHandler, CompositorState},
	delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
	delegate_seat, delegate_shm, delegate_subcompositor,
	output::{OutputHandler, OutputState},
	reexports::{calloop, calloop_wayland_source::WaylandSource},
	registry::{ProvidesRegistryState, RegistryState},
	registry_handlers,
	seat::SeatState,
	shell::{
		WaylandSurface,
		wlr_layer::{
			Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
			LayerSurfaceConfigure,
		},
	},
	shm::{
		Shm, ShmHandler,
		slot::{Buffer, SlotPool},
	},
	subcompositor::SubcompositorState,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::watch;
use wayland_client::{
	Connection, QueueHandle, delegate_noop,
	globals::registry_queue_init,
	protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
};

use wayland_protocols::wp::viewporter::client::{
	wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

pub const MAX_ROWS: usize = 8;
pub const AVATAR_EDGE: usize = 48;
const WIDTH: i32 = 248;
const ROW: i32 = 38;
const SCALE: i32 = 2;

#[derive(Clone, PartialEq, Eq)]
pub struct Monitor {
	pub name: String,
	pub label: String,
	pub logical_size: (i32, i32),
}

#[derive(PartialEq, Eq)]
pub struct Speaker {
	pub name: String,
	/// Square premultiplied native-endian ARGB pixels, at AVATAR_EDGE.
	pub avatar: Option<Arc<[u8]>>,
}

#[derive(Default, PartialEq, Eq)]
pub struct Frame {
	pub monitor: Option<String>,
	pub rows: Vec<Speaker>,
	pub position: Option<Position>,
	pub editor: Option<Editor>,
	pub appearance: Appearance,
	pub preview_appearance: Option<Appearance>,
}

#[derive(Clone, Default)]
pub enum Status {
	#[default]
	Starting,
	Available {
		monitors: Vec<Monitor>,
		editing: bool,
		fonts: Arc<[String]>,
	},
	Unavailable,
}

pub struct Overlay {
	frame: watch::Sender<Option<Arc<Frame>>>,
	pub status: watch::Receiver<Status>,
	pub edit: watch::Receiver<Option<EditUpdate>>,
	ping: calloop::ping::Ping,
}

impl Overlay {
	pub fn start(wake: impl Fn() + Send + 'static) -> Option<Self> {
		let (frame, read) = watch::channel(Some(Arc::new(Frame::default())));
		let (report, status) = watch::channel(Status::Starting);
		let (report_edit, edit) = watch::channel(None);
		let (ping, events) = calloop::ping::make_ping().ok()?;
		std::thread::Builder::new()
			.name("speaker-layer".into())
			.spawn(move || {
				if run(read, &report, report_edit, events, &wake).is_err() {
					report.send_replace(Status::Unavailable);
					wake();
				}
			})
			.ok()?;
		Some(Self {
			frame,
			status,
			edit,
			ping,
		})
	}

	pub fn update(&self, frame: Frame) {
		if self.frame.send_if_modified(|previous| {
			if previous.as_deref() == Some(&frame) {
				return false;
			}
			*previous = Some(Arc::new(frame));
			true
		}) {
			self.ping.ping();
		}
	}
}

impl Drop for Overlay {
	fn drop(&mut self) {
		self.frame.send_replace(None);
		self.ping.ping();
	}
}

struct LiveLayer {
	output: wl_output::WlOutput,
	layer: LayerSurface,
	viewport: Option<WpViewport>,
	geometry: Option<((i32, i32), (i32, i32))>,
	configured: bool,
}

impl Drop for LiveLayer {
	fn drop(&mut self) {
		if let Some(viewport) = &self.viewport {
			viewport.destroy();
		}
	}
}

struct Shell {
	registry: RegistryState,
	outputs: OutputState,
	compositor: CompositorState,
	shell: LayerShell,
	shm: Shm,
	pool: SlotPool,
	viewporter: Option<WpViewporter>,
	subcompositor: Option<SubcompositorState>,
	seats: SeatState,
	pointers: Vec<(wl_seat::WlSeat, wl_pointer::WlPointer)>,
	layer: Option<LiveLayer>,
	tree: Option<placement::Tree>,
	closed_editor: Option<u64>,
	edit_update: Option<EditUpdate>,
	transparent: Buffer,
	red: Buffer,
	preview_rows: [Speaker; 2],
	dirty: bool,
	monitors_changed: bool,
	buffers: Vec<Buffer>,
}

impl Shell {
	fn editing_available(&self) -> bool {
		self.viewporter.is_some() && self.subcompositor.is_some()
	}

	fn monitors(&self) -> impl Iterator<Item = (wl_output::WlOutput, Monitor)> + '_ {
		self.outputs.outputs().take(32).filter_map(|output| {
			let info = self.outputs.info(&output)?;
			let logical_size = info.logical_size.filter(|&(w, h)| w > 0 && h > 0)?;
			let name = info.name.filter(|name| {
				!name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
			})?;
			let label = info
				.description
				.filter(|text| {
					!text.is_empty() && text.len() <= 160 && !text.chars().any(char::is_control)
				})
				.unwrap_or_else(|| name.clone());
			Some((
				output,
				Monitor {
					name,
					label,
					logical_size,
				},
			))
		})
	}

	fn output(&self, name: &str) -> Option<(wl_output::WlOutput, Monitor)> {
		self.monitors().find(|(_, monitor)| monitor.name == name)
	}

	fn close_editor(&mut self) {
		if let Some(tree) = self.tree.take() {
			self.closed_editor = Some(tree.request.generation);
			self.edit_update = Some(tree.update(EditState::Closed));
		}
	}

	fn paint(&mut self, qh: &QueueHandle<Self>, frame: &Frame) -> Result<Option<Instant>, ()> {
		if self
			.tree
			.as_ref()
			.is_some_and(|tree| frame.editor.as_ref() != Some(&tree.request))
		{
			self.tree = None;
		}
		let editor = frame.editor.as_ref().filter(|editor| {
			self.editing_available() && self.closed_editor != Some(editor.generation)
		});
		let mut until = None;
		if let Some(editor) = editor {
			let target = self.output(&editor.monitor);
			let expired =
				matches!(editor.mode, Mode::Identify { until } if until <= Instant::now());
			if expired
				|| target.is_none()
				|| self
					.tree
					.as_ref()
					.zip(target.as_ref())
					.is_some_and(|(tree, (output, _))| &tree.output != output)
			{
				if self.tree.is_some() {
					self.close_editor();
				} else {
					self.closed_editor = Some(editor.generation);
					self.edit_update = Some(EditUpdate {
						generation: editor.generation,
						monitor: editor.monitor.clone(),
						state: EditState::Closed,
					});
				}
			} else if let Some((output, monitor)) = target {
				if self.tree.is_none() {
					let label = if monitor.label == monitor.name {
						monitor.name
					} else {
						format!("{} — {}", monitor.name, monitor.label)
					};
					self.tree = Some(placement::Tree::new(
						self,
						qh,
						output,
						editor.clone(),
						label,
					)?);
				}
				if let Mode::Identify { until: expiry } = editor.mode {
					until = Some(expiry);
				}
			}
		}
		let preview = self
			.tree
			.as_ref()
			.is_some_and(|tree| matches!(tree.request.mode, Mode::Preview { .. }));
		self.paint_live(qh, frame, preview)?;
		placement::Tree::paint(
			self,
			frame
				.preview_appearance
				.as_ref()
				.unwrap_or(&frame.appearance),
		)?;
		Ok(until)
	}

	fn paint_live(
		&mut self,
		qh: &QueueHandle<Self>,
		frame: &Frame,
		preview: bool,
	) -> Result<(), ()> {
		let target = frame
			.monitor
			.as_ref()
			.filter(|_| !preview && !frame.rows.is_empty())
			.and_then(|name| self.output(name));
		if self.layer.as_ref().map(|live| &live.output) != target.as_ref().map(|(output, _)| output)
		{
			self.layer = None;
		}
		let Some((output, monitor)) = target else {
			return Ok(());
		};
		let size = geometry::extent(monitor.logical_size);
		if self.viewporter.is_none() && size != geometry::FOOTPRINT {
			self.layer = None;
			return Ok(());
		}
		let offset = geometry::offset(monitor.logical_size, frame.position);
		if self.layer.is_none() {
			let surface = self.compositor.create_surface(qh);
			let region = smithay_client_toolkit::compositor::Region::new(&self.compositor)
				.map_err(|_| ())?;
			surface.set_input_region(Some(region.wl_region()));
			let viewport = self
				.viewporter
				.as_ref()
				.map(|v| v.get_viewport(&surface, qh, ()));
			let layer = self.shell.create_layer_surface(
				qh,
				surface,
				Layer::Overlay,
				Some("serein-speakers"),
				Some(&output),
			);
			layer.set_anchor(Anchor::TOP | Anchor::LEFT);
			layer.set_exclusive_zone(-1);
			layer.set_keyboard_interactivity(KeyboardInteractivity::None);
			self.layer = Some(LiveLayer {
				output,
				layer,
				viewport,
				geometry: None,
				configured: false,
			});
		}
		let live = self.layer.as_mut().ok_or(())?;
		let geometry_changed = live.geometry != Some((size, offset));
		if geometry_changed {
			live.layer.set_size(size.0 as u32, size.1 as u32);
			live.layer.set_margin(offset.1, 0, 0, offset.0);
			live.geometry = Some((size, offset));
		}
		if live.configured
			&& self.dirty
			&& let Some(buffer) = self
				.buffers
				.iter()
				.find(|&buffer| self.pool.canvas(buffer).is_some())
		{
			let pixels = render(&frame.rows, None, &frame.appearance, SCALE)?;
			self.pool.canvas(buffer).ok_or(())?.copy_from_slice(&pixels);
			attach_rows(buffer, live.layer.wl_surface())?;
			if let Some(viewport) = &live.viewport {
				crop(viewport, size);
			}
			live.layer.commit();
			self.dirty = false;
			return Ok(());
		}
		if geometry_changed {
			if live.configured
				&& let Some(viewport) = &live.viewport
			{
				crop(viewport, size);
			}
			live.layer.commit();
		}
		Ok(())
	}
}

fn crop(viewport: &WpViewport, size: (i32, i32)) {
	viewport.set_source(0., 0., f64::from(size.0), f64::from(size.1));
}

fn attach_rows(buffer: &Buffer, surface: &wl_surface::WlSurface) -> Result<(), ()> {
	surface.set_buffer_scale(SCALE);
	surface.damage_buffer(0, 0, WIDTH * SCALE, geometry::FOOTPRINT.1 * SCALE);
	buffer.attach_to(surface).map_err(|_| ())
}

fn run(
	mut frame: watch::Receiver<Option<Arc<Frame>>>,
	report: &watch::Sender<Status>,
	report_edit: watch::Sender<Option<EditUpdate>>,
	events: calloop::ping::PingSource,
	wake: &impl Fn(),
) -> Result<(), ()> {
	let conn = Connection::connect_to_env().map_err(|_| ())?;
	let (globals, queue) = registry_queue_init(&conn).map_err(|_| ())?;
	let qh = queue.handle();
	let shm = Shm::bind(&globals, &qh).map_err(|_| ())?;
	let width = WIDTH * SCALE;
	let height = ROW * MAX_ROWS as i32 * SCALE;
	let mut pool = SlotPool::new((width * height * 8 + 2 * 64) as usize, &shm).map_err(|_| ())?;
	let buffers = (0..2)
		.map(|_| {
			pool.create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
				.map(|(buffer, _)| buffer)
		})
		.collect::<Result<_, _>>()
		.map_err(|_| ())?;
	let mut pixel = |argb: u32| -> Result<Buffer, ()> {
		let (buffer, canvas) = pool
			.create_buffer(1, 1, 4, wl_shm::Format::Argb8888)
			.map_err(|_| ())?;
		canvas[..4].copy_from_slice(&argb.to_ne_bytes());
		Ok(buffer)
	};
	let transparent = pixel(0)?;
	let red = pixel(0xfff02d3a)?;
	let viewporter = globals.bind(&qh, 1..=1, ()).ok();
	let compositor = CompositorState::bind(&globals, &qh).map_err(|_| ())?;
	let subcompositor =
		SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh).ok();
	let mut state = Shell {
		registry: RegistryState::new(&globals),
		outputs: OutputState::new(&globals, &qh),
		compositor,
		shell: LayerShell::bind(&globals, &qh).map_err(|_| ())?,
		shm,
		pool,
		viewporter,
		subcompositor,
		seats: SeatState::new(&globals, &qh),
		pointers: Vec::new(),
		layer: None,
		tree: None,
		closed_editor: None,
		edit_update: None,
		transparent,
		red,
		preview_rows: [
			Speaker {
				name: "Alex".into(),
				avatar: Some(dummy_avatar([97, 118, 197])?),
			},
			Speaker {
				name: "Sam".into(),
				avatar: Some(dummy_avatar([161, 82, 132])?),
			},
		],
		dirty: true,
		monitors_changed: true,
		buffers,
	};
	let mut ev = calloop::EventLoop::<Shell>::try_new().map_err(|_| ())?;
	WaylandSource::new(conn, queue)
		.insert(ev.handle())
		.map_err(|_| ())?;
	ev.handle()
		.insert_source(events, |_, (), _| {})
		.map_err(|_| ())?;
	let fonts: Arc<[String]> = installed_fonts().into();
	let mut previous = Arc::new(Frame::default());
	loop {
		let Some(latest) = frame.borrow_and_update().clone() else {
			return Ok(());
		};
		if !Arc::ptr_eq(&previous, &latest) {
			state.dirty |= previous.rows != latest.rows || previous.appearance != latest.appearance;
			if previous.preview_appearance != latest.preview_appearance
				&& let Some(tree) = &mut state.tree
			{
				tree.repaint();
			}
			previous = latest;
		}
		if std::mem::take(&mut state.monitors_changed) {
			let mut monitors = state
				.monitors()
				.map(|(_, monitor)| monitor)
				.collect::<Vec<_>>();
			monitors.sort_unstable_by(|a, b| a.name.cmp(&b.name));
			report.send_replace(Status::Available {
				monitors,
				editing: state.editing_available(),
				fonts: fonts.clone(),
			});
			wake();
		}
		let until = state.paint(&qh, &previous)?;
		if let Some(edit) = state.edit_update.take()
			&& report_edit.send_if_modified(|previous| {
				if previous.as_ref() == Some(&edit) {
					return false;
				}
				*previous = Some(edit);
				true
			}) {
			wake();
		}
		ev.dispatch(
			until.map(|until| until.saturating_duration_since(Instant::now())),
			&mut state,
		)
		.map_err(|_| ())?;
	}
}

struct Decoration<'a> {
	label: &'a str,
	handle: Option<&'a str>,
}

fn dummy_avatar([r, g, b]: [u8; 3]) -> Result<Arc<[u8]>, ()> {
	let edge = AVATAR_EDGE as i32;
	let mut image =
		cairo::ImageSurface::create(cairo::Format::ARgb32, edge, edge).map_err(|_| ())?;
	{
		let cr = cairo::Context::new(&image).map_err(|_| ())?;
		cr.set_source_rgb(
			f64::from(r) / 255.,
			f64::from(g) / 255.,
			f64::from(b) / 255.,
		);
		cr.paint().map_err(|_| ())?;
		cr.set_source_rgb(0.96, 0.96, 0.98);
		cr.arc(24., 17., 8., 0., std::f64::consts::TAU);
		cr.fill().map_err(|_| ())?;
		cr.arc(24., 44., 15., 0., std::f64::consts::TAU);
		cr.fill().map_err(|_| ())?;
	}
	Ok(Arc::from(&*image.data().map_err(|_| ())?))
}

fn installed_fonts() -> Vec<String> {
	let mut fonts: Vec<_> = pangocairo::FontMap::default()
		.list_families()
		.into_iter()
		.filter_map(|family| {
			let name = family.name();
			Appearance::valid_font_family(&name).then(|| name.to_string())
		})
		.take(512)
		.collect();
	fonts.sort_unstable();
	fonts.dedup();
	fonts
}

fn rgb(color: [u8; 3]) -> [f64; 3] {
	color.map(|value| f64::from(value) / 255.)
}

fn render(
	rows: &[Speaker],
	decoration: Option<Decoration<'_>>,
	style: &Appearance,
	scale: i32,
) -> Result<Vec<u8>, ()> {
	let mut image = cairo::ImageSurface::create(
		cairo::Format::ARgb32,
		WIDTH * scale,
		ROW * MAX_ROWS as i32 * scale,
	)
	.map_err(|_| ())?;
	{
		let cr = cairo::Context::new(&image).map_err(|_| ())?;
		cr.scale(f64::from(scale), f64::from(scale));
		let layout = pangocairo::functions::create_layout(&cr);
		layout.set_font_description(Some(&pango::FontDescription::from_string(
			"Sans Semi-Bold 11",
		)));
		layout.set_single_paragraph_mode(true);
		layout.set_ellipsize(pango::EllipsizeMode::End);
		let mut row_offset = 0;
		if let Some(Decoration { label, handle }) = decoration {
			let height = placement::HANDLE_HEIGHT;
			let draw = |text: &str, top: i32, handle: bool| -> Result<(), ()> {
				layout.set_width((WIDTH - 16) * pango::SCALE);
				layout.set_text(text);
				let (_, text_height) = layout.pixel_size();
				if handle {
					cr.set_source_rgb(0.22, 0.24, 0.34);
				} else {
					cr.set_source_rgb(0.34, 0.09, 0.13);
				}
				cr.rectangle(0., f64::from(top), f64::from(WIDTH), f64::from(height));
				cr.fill().map_err(|_| ())?;
				cr.set_source_rgb(1., 1., 1.);
				cr.move_to(8., f64::from(top) + (f64::from(height - text_height) / 2.));
				pangocairo::functions::show_layout(&cr, &layout);
				Ok(())
			};
			if let Some(handle) = handle {
				draw(handle, 0, true)?;
				row_offset = height;
			}
			draw(label, row_offset + rows.len() as i32 * ROW, false)?;
		}
		let mut font = pango::FontDescription::new();
		font.set_family(style.font_family.as_deref().unwrap_or("Sans"));
		font.set_size(i32::from(style.font_size) * pango::SCALE);
		font.set_weight(match style.font_weight {
			FontWeight::Normal => pango::Weight::Normal,
			FontWeight::Semibold => pango::Weight::Semibold,
			FontWeight::Bold => pango::Weight::Bold,
		});
		layout.set_font_description(Some(&font));
		// Keep the placement footprint stable across appearance changes and speaker counts.
		let avatar = f64::from(style.avatar_size);
		let ring = avatar / 2. + 3.;
		let center = 17_f64.max(ring);
		let plate_left = if style.show_avatars { ring } else { 0. };
		let text_x = if style.show_avatars {
			i32::from(style.avatar_size) + 15
		} else {
			10
		};
		layout.set_width((WIDTH - text_x - 6) * pango::SCALE);
		let [r, g, b] = rgb(style.background);
		let background =
			cairo::SolidPattern::from_rgba(r, g, b, f64::from(style.background_opacity) / 100.);
		let [r, g, b] = rgb(style.accent);
		let accent = cairo::SolidPattern::from_rgb(r, g, b);
		let [r, g, b] = rgb(style.text);
		let text = cairo::SolidPattern::from_rgb(r, g, b);
		for (index, row) in rows.iter().take(MAX_ROWS).enumerate() {
			let y = f64::from(row_offset + index as i32 * ROW);
			layout.set_text(&row.name);
			let (label_width, label_height) = layout.pixel_size();
			let plate_right = (text_x + label_width + 7).min(WIDTH);
			cr.save().map_err(|_| ())?;
			cr.rectangle(0., y, f64::from(WIDTH), f64::from(ROW));
			cr.clip();
			let plate_height = f64::from((label_height + 4).clamp(29, ROW));
			let plate_y =
				y + (center - (plate_height - 1.) / 2.).clamp(0., f64::from(ROW) - plate_height);
			cr.set_source(&background).map_err(|_| ())?;
			cr.rectangle(
				plate_left,
				plate_y,
				f64::from(plate_right) - plate_left,
				plate_height,
			);
			cr.fill().map_err(|_| ())?;
			cr.set_source(&accent).map_err(|_| ())?;
			if style.show_avatars {
				cr.arc(ring, y + center, ring, 0., std::f64::consts::TAU);
				cr.fill().map_err(|_| ())?;
				paint_avatar(&cr, row.avatar.as_deref(), avatar, ring, y + center)?;
			} else {
				cr.rectangle(0., plate_y, 3., plate_height);
				cr.fill().map_err(|_| ())?;
			}
			cr.set_source(&text).map_err(|_| ())?;
			let label_y = (center - f64::from(label_height) / 2.)
				.clamp(0., f64::from((ROW - label_height).max(0)));
			cr.move_to(f64::from(text_x), y + label_y);
			pangocairo::functions::show_layout(&cr, &layout);
			cr.restore().map_err(|_| ())?;
		}
	}
	Ok(image.data().map_err(|_| ())?.to_vec())
}

fn paint_avatar(
	cr: &cairo::Context,
	pixels: Option<&[u8]>,
	edge: f64,
	x: f64,
	y: f64,
) -> Result<(), ()> {
	let radius = edge / 2.;
	cr.save().map_err(|_| ())?;
	cr.arc(x, y, radius, 0., std::f64::consts::TAU);
	cr.clip();
	cr.set_source_rgb(0.16, 0.22, 0.23);
	cr.paint().map_err(|_| ())?;
	if let Some(pixels) = pixels {
		let face = cairo::ImageSurface::create_for_data(
			pixels.to_vec(),
			cairo::Format::ARgb32,
			AVATAR_EDGE as i32,
			AVATAR_EDGE as i32,
			(AVATAR_EDGE * 4) as i32,
		)
		.map_err(|_| ())?;
		cr.translate(x - radius, y - radius);
		cr.scale(edge / AVATAR_EDGE as f64, edge / AVATAR_EDGE as f64);
		cr.set_source_surface(face, 0., 0.).map_err(|_| ())?;
		cr.paint().map_err(|_| ())?;
	}
	cr.restore().map_err(|_| ())
}

impl CompositorHandler for Shell {
	fn scale_factor_changed(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		_: &wl_surface::WlSurface,
		_: i32,
	) {
	}
	fn transform_changed(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		_: &wl_surface::WlSurface,
		_: wl_output::Transform,
	) {
	}
	fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
	fn surface_enter(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		_: &wl_surface::WlSurface,
		_: &wl_output::WlOutput,
	) {
	}
	fn surface_leave(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		_: &wl_surface::WlSurface,
		_: &wl_output::WlOutput,
	) {
	}
}
impl OutputHandler for Shell {
	fn output_state(&mut self) -> &mut OutputState {
		&mut self.outputs
	}
	fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
		self.monitors_changed = true;
	}
	fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
		self.monitors_changed = true;
	}
	fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
		self.monitors_changed = true;
	}
}
impl LayerShellHandler for Shell {
	fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
		if self.layer.as_ref().is_some_and(|live| &live.layer == layer) {
			self.layer = None;
		}
		if self.tree.as_ref().is_some_and(|tree| &tree.layer == layer) {
			self.close_editor();
		}
	}
	fn configure(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		layer: &LayerSurface,
		configure: LayerSurfaceConfigure,
		_: u32,
	) {
		if let Some(live) = self.layer.as_mut().filter(|live| &live.layer == layer)
			&& !live.configured
		{
			live.configured = true;
			self.dirty = true;
		}
		if let Some(tree) = self.tree.as_mut().filter(|tree| &tree.layer == layer) {
			let (width, height) = configure.new_size;
			if let Some(update) = tree.configure((width as i32, height as i32)) {
				self.edit_update = Some(update);
			}
		}
	}
}
impl ShmHandler for Shell {
	fn shm_state(&mut self) -> &mut Shm {
		&mut self.shm
	}
}
impl ProvidesRegistryState for Shell {
	fn registry(&mut self) -> &mut RegistryState {
		&mut self.registry
	}
	registry_handlers![OutputState, SeatState];
}
delegate_compositor!(Shell);
delegate_output!(Shell);
delegate_layer!(Shell);
delegate_shm!(Shell);
delegate_registry!(Shell);
delegate_subcompositor!(Shell);
delegate_seat!(Shell);
delegate_pointer!(Shell);
delegate_noop!(Shell: ignore WpViewporter);
delegate_noop!(Shell: ignore WpViewport);

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hidden_avatars_do_not_affect_rendered_rows() {
		let mut rows = [Speaker {
			name: "Synthetic speaker".into(),
			avatar: Some(dummy_avatar([220, 10, 180]).unwrap()),
		}];
		let visible = Appearance::default();
		let hidden = Appearance {
			show_avatars: false,
			..Appearance::default()
		};
		let visible_with_avatar = render(&rows, None, &visible, 1).unwrap();
		let hidden_with_avatar = render(&rows, None, &hidden, 1).unwrap();
		rows[0].avatar = None;
		assert_ne!(
			visible_with_avatar,
			render(&rows, None, &visible, 1).unwrap()
		);
		assert_eq!(hidden_with_avatar, render(&rows, None, &hidden, 1).unwrap());
	}
}
