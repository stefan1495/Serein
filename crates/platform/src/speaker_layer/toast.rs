use super::*;
use model::voice_overlay::ToastCorner;

pub(super) const SIZE: (i32, i32) = (320, 104);

#[derive(Clone, PartialEq, Eq)]
pub struct Toast {
	pub serial: u64,
	pub output: u32,
	pub corner: ToastCorner,
	pub position: Option<Position>,
	pub until: Instant,
	/// Renewable evidence lease, separate from this message's display deadline.
	pub foreground_until: Instant,
	pub foreground_generation: u64,
	pub sender: String,
	pub channel: String,
	pub excerpt: Option<String>,
}

impl Toast {
	pub fn active_at(&self, now: Instant) -> bool {
		now < self.until && now < self.foreground_until
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToastRejection {
	pub serial: u64,
	/// Native geometry rejected this still-active candidate before it ever attached.
	pub fallback: bool,
}

enum PaintError {
	Geometry,
	Unavailable,
}

impl PaintError {
	fn rejection(
		self,
		toast: &Toast,
		attached_serial: Option<u64>,
		now: Instant,
	) -> ToastRejection {
		ToastRejection {
			serial: toast.serial,
			fallback: matches!(self, Self::Geometry)
				&& attached_serial != Some(toast.serial)
				&& toast.active_at(now),
		}
	}
}

pub(super) fn placement(
	size: (i32, i32),
	corner: ToastCorner,
	position: Option<Position>,
	voice: Option<Option<Position>>,
) -> Option<(i32, i32)> {
	let point = geometry::notification_offset(size, corner, position)?;
	if voice.is_some_and(|voice| geometry::overlap(geometry::offset(size, voice), point)) {
		return None;
	}
	Some(point)
}

impl Monitor {
	pub fn can_show_toast(
		&self,
		corner: ToastCorner,
		position: Option<Position>,
		voice: Option<Option<Position>>,
	) -> bool {
		self.notifications_supported
			&& placement(self.logical_size, corner, position, voice).is_some()
	}
}

pub(super) struct Surface {
	pub layer: LayerSurface,
	pub output: u32,
	pub configured: bool,
	pub candidate_serial: u64,
	foreground_until: Instant,
	attached: Option<AttachedToast>,
	placement: Option<Placement>,
}

struct AttachedToast {
	serial: u64,
	until: Instant,
	foreground_generation: u64,
}

impl AttachedToast {
	fn next_deadline(attached: Option<&Self>, candidate: &Toast) -> Instant {
		let deadline = candidate.until.min(candidate.foreground_until);
		attached.map_or(deadline, |shown| shown.until.min(deadline))
	}

	fn must_unmap_before(&self, candidate: &Toast, now: Instant) -> bool {
		self.until <= now || self.foreground_generation != candidate.foreground_generation
	}

	fn try_commit(
		attached: &mut Option<Self>,
		candidate: &Toast,
		commit: impl FnOnce() -> Result<bool, ()>,
	) -> Result<bool, ()> {
		if attached
			.as_ref()
			.is_some_and(|shown| shown.serial == candidate.serial)
			|| !commit()?
		{
			return Ok(false);
		}
		*attached = Some(Self {
			serial: candidate.serial,
			until: candidate.until,
			foreground_generation: candidate.foreground_generation,
		});
		Ok(true)
	}
}

#[derive(PartialEq, Eq)]
struct Placement {
	size: (i32, i32),
	point: (i32, i32),
	corner: ToastCorner,
}

impl Surface {
	fn new(
		shell: &Shell,
		qh: &QueueHandle<Shell>,
		output: &wl_output::WlOutput,
		toast: &Toast,
	) -> Result<Self, ()> {
		let surface = shell.compositor.create_surface(qh);
		let region =
			smithay_client_toolkit::compositor::Region::new(&shell.compositor).map_err(|_| ())?;
		surface.set_input_region(Some(region.wl_region()));
		let layer = shell.shell.create_layer_surface(
			qh,
			surface,
			Layer::Overlay,
			Some("serein-notifications"),
			Some(output),
		);
		layer.set_size(SIZE.0 as u32, SIZE.1 as u32);
		layer.set_exclusive_zone(-1);
		layer.set_keyboard_interactivity(KeyboardInteractivity::None);
		Ok(Self {
			layer,
			output: toast.output,
			configured: false,
			candidate_serial: toast.serial,
			foreground_until: toast.foreground_until,
			attached: None,
			placement: None,
		})
	}

	fn paint(
		&mut self,
		pool: &mut SlotPool,
		buffers: &[Buffer],
		monitor: &Monitor,
		toast: &Toast,
		point: (i32, i32),
	) -> Result<(), ()> {
		self.candidate_serial = toast.serial;
		self.foreground_until = toast.foreground_until;
		let placement = Placement {
			size: monitor.logical_size,
			point,
			corner: toast.corner,
		};
		let changed = self.placement.as_ref() != Some(&placement);
		if changed {
			let (w, h) = monitor.logical_size;
			let (x, y) = point;
			let (anchor, margins) = match toast.corner {
				ToastCorner::TopLeft => (Anchor::TOP | Anchor::LEFT, (y, 0, 0, x)),
				ToastCorner::TopRight => (Anchor::TOP | Anchor::RIGHT, (y, w - x - SIZE.0, 0, 0)),
				ToastCorner::BottomLeft => {
					(Anchor::BOTTOM | Anchor::LEFT, (0, 0, h - y - SIZE.1, x))
				}
				ToastCorner::BottomRight => (
					Anchor::BOTTOM | Anchor::RIGHT,
					(0, w - x - SIZE.0, h - y - SIZE.1, 0),
				),
			};
			self.layer.set_anchor(anchor);
			self.layer
				.set_margin(margins.0, margins.1, margins.2, margins.3);
			self.placement = Some(placement);
		}
		let committed = AttachedToast::try_commit(&mut self.attached, toast, || {
			if !self.configured {
				return Ok(false);
			}
			let Some(buffer) = buffers.iter().find(|&buffer| pool.canvas(buffer).is_some()) else {
				return Ok(false);
			};
			let pixels = render(
				&toast.sender,
				&toast.channel,
				toast.excerpt.as_deref(),
				false,
			)?;
			pool.canvas(buffer).ok_or(())?.copy_from_slice(&pixels);
			if !toast.active_at(Instant::now()) {
				return Err(());
			}
			let surface = self.layer.wl_surface();
			surface.set_buffer_scale(SCALE);
			surface.damage_buffer(0, 0, SIZE.0 * SCALE, SIZE.1 * SCALE);
			buffer.attach_to(surface).map_err(|_| ())?;
			self.layer.commit();
			Ok(true)
		})?;
		if !committed && changed {
			self.layer.commit();
		}
		Ok(())
	}
}

impl Shell {
	pub(super) fn paint_toast(
		&mut self,
		qh: &QueueHandle<Self>,
		frame: &Frame,
		now: Instant,
	) -> Option<Instant> {
		let Some(toast) = &frame.toast else {
			self.toast = None;
			return None;
		};
		let attached_serial = self
			.toast
			.as_ref()
			.and_then(|surface| surface.attached.as_ref())
			.map(|shown| shown.serial);
		if let Some(surface) = self
			.toast
			.as_ref()
			.filter(|surface| surface.foreground_until <= now)
		{
			self.rejected_toast = Some(ToastRejection {
				serial: surface.candidate_serial,
				fallback: false,
			});
			self.toast = None;
		}
		if self
			.rejected_toast
			.is_some_and(|last| last.serial == toast.serial)
		{
			self.toast = None;
			return None;
		}
		let error = if toast.active_at(now) && frame.editor.is_none() {
			match self.try_paint_toast(qh, frame, toast, now) {
				Ok(until) => return Some(until),
				Err(error) => error,
			}
		} else {
			PaintError::Unavailable
		};
		self.toast = None;
		self.rejected_toast = Some(error.rejection(toast, attached_serial, Instant::now()));
		None
	}

	fn try_paint_toast(
		&mut self,
		qh: &QueueHandle<Self>,
		frame: &Frame,
		toast: &Toast,
		now: Instant,
	) -> Result<Instant, PaintError> {
		if self.closed_toast_output == Some(toast.output) {
			return Err(PaintError::Unavailable);
		}
		let (output, monitor) = self
			.monitors()
			.find(|(_, monitor)| monitor.id == toast.output)
			.ok_or(PaintError::Unavailable)?;
		let voice = self
			.layer
			.as_ref()
			.filter(|voice| voice.output == output)
			.map(|_| frame.position);
		let point = placement(monitor.logical_size, toast.corner, toast.position, voice)
			.ok_or(PaintError::Geometry)?;
		if self.toast.as_ref().is_some_and(|surface| {
			surface.output != toast.output
				|| surface
					.attached
					.as_ref()
					.is_some_and(|shown| shown.must_unmap_before(toast, now))
		}) {
			// A latest-frame replacement can hide a clear. Never carry pixels across focus generations.
			self.toast = None;
		}
		if self.toast.is_none() {
			self.toast =
				Some(Surface::new(self, qh, &output, toast).map_err(|_| PaintError::Unavailable)?);
		}
		let surface = self.toast.as_mut().ok_or(PaintError::Unavailable)?;
		surface
			.paint(&mut self.pool, &self.toast_buffers, &monitor, toast, point)
			.map_err(|_| PaintError::Unavailable)?;
		Ok(AttachedToast::next_deadline(
			surface.attached.as_ref(),
			toast,
		))
	}
}

pub(super) fn render_preview() -> Result<Vec<u8>, ()> {
	render(
		"Jamie",
		"# game-night",
		Some("Ready for the next round?"),
		true,
	)
}

fn render(
	sender: &str,
	channel: &str,
	excerpt: Option<&str>,
	preview: bool,
) -> Result<Vec<u8>, ()> {
	let mut image =
		cairo::ImageSurface::create(cairo::Format::ARgb32, SIZE.0 * SCALE, SIZE.1 * SCALE)
			.map_err(|_| ())?;
	{
		let cr = cairo::Context::new(&image).map_err(|_| ())?;
		cr.scale(f64::from(SCALE), f64::from(SCALE));
		cr.set_source_rgba(0.09, 0.10, 0.14, 0.96);
		cr.paint().map_err(|_| ())?;
		let layout = pangocairo::functions::create_layout(&cr);
		layout.set_width((SIZE.0 - 24) * pango::SCALE);
		layout.set_ellipsize(pango::EllipsizeMode::End);
		let draw = |text: &str, y, font: &str, lines: i32, shade| {
			layout.set_single_paragraph_mode(lines == 1);
			layout.set_height(-lines);
			layout.set_font_description(Some(&pango::FontDescription::from_string(font)));
			layout.set_text(text);
			cr.set_source_rgb(shade, shade, shade);
			cr.move_to(12., y);
			pangocairo::functions::show_layout(&cr, &layout);
		};
		let y = if preview {
			f64::from(super::placement::HANDLE_HEIGHT) + 5.
		} else if excerpt.is_some() {
			8.
		} else {
			29.
		};
		draw(sender, y, "Sans Semi-Bold 12", 1, 0.99);
		draw(channel, y + 22., "Sans 10", 1, 0.85);
		if let Some(excerpt) = excerpt {
			draw(
				excerpt,
				y + 46.,
				"Sans 11",
				if preview { 1 } else { 2 },
				0.96,
			);
		}
	}
	Ok(image.data().map_err(|_| ())?.to_vec())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn attached_toast_expires_while_replacement_waits_for_shm() {
		let start = Instant::now();
		let old_until = start + std::time::Duration::from_secs(4);
		let replacement = Toast {
			serial: 2,
			output: 1,
			corner: ToastCorner::TopRight,
			position: None,
			until: old_until + std::time::Duration::from_secs(2),
			foreground_until: old_until + std::time::Duration::from_secs(1),
			foreground_generation: 1,
			sender: String::new(),
			channel: String::new(),
			excerpt: None,
		};
		let old = Toast {
			serial: 1,
			until: old_until,
			..replacement.clone()
		};
		let mut attached = None;
		assert!(AttachedToast::try_commit(&mut attached, &old, || Ok(true)).unwrap());
		assert!(!AttachedToast::try_commit(&mut attached, &replacement, || Ok(false)).unwrap());
		let shown = attached.as_ref().unwrap();
		assert_eq!(
			shown.serial, old.serial,
			"busy SHM leaves the old buffer attached"
		);
		assert_eq!(
			AttachedToast::next_deadline(attached.as_ref(), &replacement),
			old_until
		);
		assert!(!shown.must_unmap_before(&replacement, start));
		let after_focus_loss = Toast {
			foreground_generation: 2,
			..replacement.clone()
		};
		assert!(shown.must_unmap_before(&after_focus_loss, start));
		assert!(shown.must_unmap_before(&replacement, old_until));
		assert!(replacement.active_at(old_until));

		// The expired mapping is dropped; its replacement waits for configure and release.
		attached = None;
		assert!(!AttachedToast::try_commit(&mut attached, &replacement, || Ok(false)).unwrap());
		assert_eq!(
			AttachedToast::next_deadline(attached.as_ref(), &replacement),
			replacement.foreground_until
		);
		assert!(AttachedToast::try_commit(&mut attached, &replacement, || Ok(true)).unwrap());
		assert_eq!(attached.as_ref().unwrap().serial, replacement.serial);
		assert!(
			attached
				.as_ref()
				.unwrap()
				.must_unmap_before(&replacement, replacement.until)
		);
		assert!(!replacement.active_at(replacement.foreground_until));
		let renewed = Toast {
			foreground_until: replacement.until + std::time::Duration::from_secs(1),
			..replacement
		};
		assert!(!AttachedToast::try_commit(&mut attached, &renewed, || unreachable!()).unwrap());
		assert_eq!(
			AttachedToast::next_deadline(attached.as_ref(), &renewed),
			renewed.until
		);
		assert!(!renewed.active_at(renewed.until));
	}

	#[test]
	fn geometry_rejection_allows_fallback_only_for_the_current_unattached_active_toast() {
		let now = Instant::now();
		let future = now + std::time::Duration::from_secs(1);
		let candidate = Toast {
			serial: 2,
			output: 1,
			corner: ToastCorner::default(),
			position: None,
			until: future,
			foreground_until: future,
			foreground_generation: 1,
			sender: String::new(),
			channel: String::new(),
			excerpt: None,
		};
		assert_eq!(
			PaintError::Geometry.rejection(&candidate, Some(1), now),
			ToastRejection {
				serial: candidate.serial,
				fallback: true
			}
		);
		assert!(
			!PaintError::Geometry
				.rejection(&candidate, Some(candidate.serial), now)
				.fallback
		);
		assert!(
			!PaintError::Unavailable
				.rejection(&candidate, None, now)
				.fallback
		);
		let lost_foreground = Toast {
			foreground_until: now,
			..candidate.clone()
		};
		assert!(
			!PaintError::Geometry
				.rejection(&lost_foreground, None, now)
				.fallback
		);
		let expired = Toast {
			until: now,
			..candidate
		};
		assert!(!PaintError::Geometry.rejection(&expired, None, now).fallback);
	}

	#[test]
	fn unset_toast_preserves_legacy_corners_and_overlap_never_repositions_it() {
		let size = (960, 720);
		for (corner, point) in [
			(ToastCorner::TopLeft, (12, 12)),
			(ToastCorner::TopRight, (628, 12)),
			(ToastCorner::BottomLeft, (12, 604)),
			(ToastCorner::BottomRight, (628, 604)),
		] {
			assert_eq!(placement(size, corner, None, None), Some(point));
		}
		let voice = Some(Some(Position { x: 10_000, y: 0 }));
		assert_eq!(placement(size, ToastCorner::TopRight, None, voice), None);
		assert_eq!(
			placement(size, ToastCorner::BottomLeft, None, voice),
			Some((12, 604))
		);
		let explicit = Some(Position {
			x: 10_000,
			y: 10_000,
		});
		assert_eq!(
			placement(size, ToastCorner::TopLeft, explicit, voice),
			Some((640, 616))
		);
		assert_eq!(
			placement((319, 720), ToastCorner::TopLeft, explicit, None),
			None
		);
	}
}
