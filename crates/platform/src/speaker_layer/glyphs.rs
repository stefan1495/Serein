use super::{SpeakerStatus, cairo};
use std::cell::LazyCell;

const ATLAS: &[u8] = include_bytes!("../../../../assets/icons/atlas.png");
const INDEX: &str = include_str!("../../../../assets/icons/index.tsv");
const CELL: u32 = 64;
const COLUMNS: u32 = 8;
pub(super) const SLOT: i32 = 20;
const GLYPH: f64 = 18.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Glyph {
	MicrophoneSlash,
	HeadphonesSlash,
	Speaker,
}

pub(super) fn status_glyphs(status: SpeakerStatus) -> [Option<Glyph>; 2] {
	[
		if status.deafened {
			Some(Glyph::HeadphonesSlash)
		} else if status.muted {
			Some(Glyph::MicrophoneSlash)
		} else {
			None
		},
		status.locally_muted.then_some(Glyph::Speaker),
	]
}

struct Masks([cairo::ImageSurface; 3]);

impl Masks {
	fn load() -> Result<Self, ()> {
		// The indexed PNG's tRNS supplies anti-aliased alpha. Keep only these three cells.
		let atlas = image::load_from_memory_with_format(ATLAS, image::ImageFormat::Png)
			.map_err(|_| ())?
			.into_rgba8();
		let stride = cairo::Format::A8.stride_for_width(CELL).map_err(|_| ())?;
		let mask = |name: &str| {
			let index: u32 = INDEX
				.lines()
				.find_map(|line| {
					let (candidate, index) = line.split_once('\t')?;
					if candidate == name {
						index.parse().ok()
					} else {
						None
					}
				})
				.ok_or(())?;
			let row = index / COLUMNS;
			if atlas.width() != COLUMNS * CELL || row >= atlas.height() / CELL {
				return Err(());
			}
			let (left, top) = ((index % COLUMNS) * CELL, row * CELL);
			let mut alpha = vec![0; stride as usize * CELL as usize];
			for y in 0..CELL {
				for x in 0..CELL {
					alpha[y as usize * stride as usize + x as usize] =
						atlas.get_pixel(left + x, top + y).0[3];
				}
			}
			cairo::ImageSurface::create_for_data(
				alpha,
				cairo::Format::A8,
				CELL as i32,
				CELL as i32,
				stride,
			)
			.map_err(|_| ())
		};
		Ok(Self([
			mask("microphone-slash")?,
			mask("headphones-slash")?,
			mask("speaker-high")?,
		]))
	}
}

thread_local! {
	static MASKS: LazyCell<Result<Masks, ()>> = const { LazyCell::new(Masks::load) };
}

pub(super) fn draw(cr: &cairo::Context, glyph: Glyph, left: f64, center: f64) -> Result<(), ()> {
	MASKS.with(|masks| {
		let masks = (**masks).as_ref().map_err(|_| ())?;
		cr.save().map_err(|_| ())?;
		cr.set_source_rgb(0.12, 0.12, 0.16);
		cr.arc(
			left + GLYPH / 2.,
			center,
			GLYPH / 2.,
			0.,
			std::f64::consts::TAU,
		);
		cr.fill().map_err(|_| ())?;
		if glyph == Glyph::Speaker {
			cr.set_source_rgb(1., 0.53, 0.56);
		} else {
			cr.set_source_rgb(0.94, 0.94, 0.97);
		}
		cr.translate(left, center - GLYPH / 2.);
		cr.scale(GLYPH / f64::from(CELL), GLYPH / f64::from(CELL));
		cr.mask_surface(&masks.0[glyph as usize], 0., 0.)
			.map_err(|_| ())?;
		cr.restore().map_err(|_| ())
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn native_status_glyphs_prioritize_deafen_without_hiding_local_mute() {
		let status = SpeakerStatus {
			speaking: true,
			muted: true,
			deafened: true,
			locally_muted: true,
		};
		assert_eq!(
			status_glyphs(status),
			[Some(Glyph::HeadphonesSlash), Some(Glyph::Speaker)]
		);
		assert_eq!(
			status_glyphs(SpeakerStatus {
				deafened: false,
				..status
			}),
			[Some(Glyph::MicrophoneSlash), Some(Glyph::Speaker)]
		);
	}
}
