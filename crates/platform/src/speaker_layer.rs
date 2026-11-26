//! A click-through Wayland layer surface. No input, audio or IPC listeners.
use gtk4::{cairo, pango};
use smithay_client_toolkit::{
	compositor::{CompositorHandler, CompositorState},
	delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
	output::{OutputHandler, OutputState},
	reexports::{calloop, calloop_wayland_source::WaylandSource},
	registry::{ProvidesRegistryState, RegistryState},
	registry_handlers,
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
};
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;
use wayland_client::{
	Connection, QueueHandle,
	globals::registry_queue_init,
	protocol::{wl_output, wl_shm, wl_surface},
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
}

#[derive(Clone, Default)]
pub enum Status {
	#[default]
	Starting,
	Available(Vec<Monitor>),
	Unavailable,
}

pub struct Overlay {
	frame: watch::Sender<Arc<Frame>>,
	pub status: watch::Receiver<Status>,
	ping: calloop::ping::Ping,
}

impl Overlay {
	pub fn start(wake: impl Fn() + Send + 'static) -> Option<Self> {
		let (frame, read) = watch::channel(Arc::new(Frame::default()));
		let (report, status) = watch::channel(Status::Starting);
		let (ping, events) = calloop::ping::make_ping().ok()?;
		std::thread::Builder::new()
			.name("speaker-layer".into())
			.spawn(move || {
				if run(read, &report, events, &wake).is_err() {
					report.send_replace(Status::Unavailable);
					wake();
				}
			})
			.ok()?;
		Some(Self {
			frame,
			status,
			ping,
		})
	}

	pub fn update(&self, frame: Frame) {
		if self.frame.send_if_modified(|previous| {
			if **previous == frame {
				return false;
			}
			*previous = Arc::new(frame);
			true
		}) {
			self.ping.ping();
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
	layer: Option<(wl_output::WlOutput, LayerSurface)>,
	configured: bool,
	dirty: bool,
	monitors_changed: bool,
	buffers: Vec<Buffer>,
}

impl Shell {
	fn monitors(&self) -> impl Iterator<Item = (wl_output::WlOutput, Monitor)> + '_ {
		self.outputs.outputs().take(32).filter_map(|output| {
			let info = self.outputs.info(&output)?;
			let name = info.name.filter(|name| {
				!name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
			})?;
			let label = info
				.description
				.filter(|text| !text.is_empty() && text.len() <= 160)
				.unwrap_or_else(|| name.clone());
			Some((output, Monitor { name, label }))
		})
	}

	fn paint(&mut self, qh: &QueueHandle<Self>, frame: &Frame) -> Result<(), ()> {
		let output = frame
			.monitor
			.as_ref()
			.filter(|_| !frame.rows.is_empty())
			.and_then(|selected| {
				self.monitors()
					.find_map(|(output, item)| (&item.name == selected).then_some(output))
			});
		if self.layer.as_ref().map(|(output, _)| output) != output.as_ref() {
			self.layer = None;
			self.configured = false;
			if let Some(output) = output {
				let surface = self.compositor.create_surface(qh);
				let region = smithay_client_toolkit::compositor::Region::new(&self.compositor)
					.map_err(|_| ())?;
				surface.set_input_region(Some(region.wl_region()));
				let layer = self.shell.create_layer_surface(
					qh,
					surface,
					Layer::Overlay,
					Some("serein-speakers"),
					Some(&output),
				);
				layer.set_anchor(Anchor::TOP | Anchor::LEFT);
				layer.set_margin(12, 0, 0, 12);
				layer.set_exclusive_zone(-1);
				layer.set_keyboard_interactivity(KeyboardInteractivity::None);
				layer.set_size(WIDTH as u32, (ROW * MAX_ROWS as i32) as u32);
				layer.commit();
				self.layer = Some((output, layer));
			}
		}
		if !self.configured || !self.dirty {
			return Ok(());
		}
		let Some((_, layer)) = &self.layer else {
			return Ok(());
		};
		let Some(buffer) = self
			.buffers
			.iter()
			.find(|&buffer| self.pool.canvas(buffer).is_some())
		else {
			return Ok(());
		};
		let (width, height) = (WIDTH * SCALE, ROW * MAX_ROWS as i32 * SCALE);
		let canvas = self.pool.canvas(buffer).ok_or(())?;
		let pixels = render(frame, SCALE)?;
		canvas.copy_from_slice(&pixels);
		layer.wl_surface().set_buffer_scale(SCALE);
		layer.wl_surface().damage_buffer(0, 0, width, height);
		buffer.attach_to(layer.wl_surface()).map_err(|_| ())?;
		layer.commit();
		self.dirty = false;
		Ok(())
	}
}

fn run(
	mut frame: watch::Receiver<Arc<Frame>>,
	report: &watch::Sender<Status>,
	events: calloop::ping::PingSource,
	wake: &impl Fn(),
) -> Result<(), ()> {
	let conn = Connection::connect_to_env().map_err(|_| ())?;
	let (globals, queue) = registry_queue_init(&conn).map_err(|_| ())?;
	let qh = queue.handle();
	let shm = Shm::bind(&globals, &qh).map_err(|_| ())?;
	let width = WIDTH * SCALE;
	let height = ROW * MAX_ROWS as i32 * SCALE;
	let mut pool = SlotPool::new((width * height * 8) as usize, &shm).map_err(|_| ())?;
	let buffers = (0..2)
		.map(|_| {
			pool.create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
				.map(|(buffer, _)| buffer)
		})
		.collect::<Result<_, _>>()
		.map_err(|_| ())?;
	let mut state = Shell {
		registry: RegistryState::new(&globals),
		outputs: OutputState::new(&globals, &qh),
		compositor: CompositorState::bind(&globals, &qh).map_err(|_| ())?,
		shell: LayerShell::bind(&globals, &qh).map_err(|_| ())?,
		shm,
		pool,
		layer: None,
		configured: false,
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
	let mut previous = Arc::new(Frame::default());
	loop {
		frame.has_changed().map_err(|_| ())?;
		let latest = frame.borrow_and_update().clone();
		if !Arc::ptr_eq(&previous, &latest) {
			state.dirty = true;
			previous = latest;
		}
		if std::mem::take(&mut state.monitors_changed) {
			let mut monitors = state
				.monitors()
				.map(|(_, monitor)| monitor)
				.collect::<Vec<_>>();
			monitors.sort_unstable_by(|a, b| a.name.cmp(&b.name));
			report.send_replace(Status::Available(monitors));
			wake();
		}
		state.paint(&qh, &previous)?;
		ev.dispatch(Duration::from_secs(1), &mut state)
			.map_err(|_| ())?;
	}
}

fn render(frame: &Frame, scale: i32) -> Result<Vec<u8>, ()> {
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
		layout.set_width((WIDTH - 49) * pango::SCALE);
		layout.set_single_paragraph_mode(true);
		layout.set_ellipsize(pango::EllipsizeMode::End);
		for (index, row) in frame.rows.iter().take(MAX_ROWS).enumerate() {
			let y = (index as f64) * f64::from(ROW);
			layout.set_text(&row.name);
			let (label_width, label_height) = layout.pixel_size();
			cr.set_source_rgba(0.09, 0.09, 0.11, 0.94);
			cr.rectangle(
				17.,
				y + 3.,
				f64::from((label_width + 33).min(WIDTH - 17)),
				29.,
			);
			cr.fill().map_err(|_| ())?;
			cr.set_source_rgb(0.2, 0.8, 0.47);
			cr.arc(17., y + 17., 17., 0., std::f64::consts::TAU);
			cr.fill().map_err(|_| ())?;
			cr.save().map_err(|_| ())?;
			cr.arc(17., y + 17., 14., 0., std::f64::consts::TAU);
			cr.clip();
			cr.set_source_rgb(0.16, 0.22, 0.23);
			cr.paint().map_err(|_| ())?;
			if let Some(avatar) = &row.avatar {
				let face = cairo::ImageSurface::create_for_data(
					avatar.to_vec(),
					cairo::Format::ARgb32,
					AVATAR_EDGE as i32,
					AVATAR_EDGE as i32,
					(AVATAR_EDGE * 4) as i32,
				)
				.map_err(|_| ())?;
				cr.translate(3., y + 3.);
				cr.scale(28. / AVATAR_EDGE as f64, 28. / AVATAR_EDGE as f64);
				cr.set_source_surface(face, 0., 0.).map_err(|_| ())?;
				cr.paint().map_err(|_| ())?;
			}
			cr.restore().map_err(|_| ())?;
			cr.set_source_rgb(1., 1., 1.);
			cr.move_to(43., y + (34. - f64::from(label_height)) / 2.);
			pangocairo::functions::show_layout(&cr, &layout);
		}
	}
	Ok(image.data().map_err(|_| ())?.to_vec())
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
		if self
			.layer
			.as_ref()
			.is_some_and(|(_, current)| current == layer)
		{
			self.layer = None;
			self.configured = false;
		}
	}
	fn configure(
		&mut self,
		_: &Connection,
		_: &QueueHandle<Self>,
		layer: &LayerSurface,
		_: LayerSurfaceConfigure,
		_: u32,
	) {
		if self
			.layer
			.as_ref()
			.is_some_and(|(_, current)| current == layer)
		{
			self.configured = true;
			self.dirty = true;
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
	registry_handlers![OutputState];
}
delegate_compositor!(Shell);
delegate_output!(Shell);
delegate_layer!(Shell);
delegate_shm!(Shell);
delegate_registry!(Shell);
