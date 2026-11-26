use model::voice_overlay::Position;

pub(super) const FOOTPRINT: (i32, i32) = (super::WIDTH, super::ROW * super::MAX_ROWS as i32);
const DEFAULT_INSET: i32 = 12;
const DENOMINATOR: i64 = Position::DENOMINATOR as i64;

pub(super) fn extent(size: (i32, i32)) -> (i32, i32) {
	(size.0.min(FOOTPRINT.0), size.1.min(FOOTPRINT.1))
}

fn travel(size: (i32, i32)) -> (i32, i32) {
	((size.0 - FOOTPRINT.0).max(0), (size.1 - FOOTPRINT.1).max(0))
}

pub(super) fn offset(size: (i32, i32), position: Option<Position>) -> (i32, i32) {
	fn axis(travel: i32, fraction: Option<u16>) -> i32 {
		fraction.map_or(travel.min(DEFAULT_INSET), |fraction| {
			((i64::from(travel) * i64::from(fraction.min(Position::DENOMINATOR)) + DENOMINATOR / 2)
				/ DENOMINATOR) as i32
		})
	}
	let (x, y) = travel(size);
	(
		axis(x, position.map(|p| p.x)),
		axis(y, position.map(|p| p.y)),
	)
}

pub(super) fn drag(
	size: (i32, i32),
	previous: Option<Position>,
	wanted: (i32, i32),
) -> (Option<Position>, (i32, i32)) {
	fn axis(travel: i32, wanted: i32, previous: Option<u16>) -> u16 {
		if travel == 0 {
			return previous.unwrap_or(0);
		}
		let wanted = i64::from(wanted.clamp(0, travel));
		((wanted * DENOMINATOR + i64::from(travel) / 2) / i64::from(travel)) as u16
	}
	let (x, y) = travel(size);
	let position = if (x, y) == (0, 0) {
		previous
	} else {
		Some(Position {
			x: axis(x, wanted.0, previous.map(|p| p.x)),
			y: axis(y, wanted.1, previous.map(|p| p.y)),
		})
	};
	(position, offset(size, position))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn legacy_inset_clamps_and_small_outputs_preserve_saved_fractions() {
		assert_eq!(offset((960, 720), None), (12, 12));
		assert_eq!(offset((250, 48), None), (2, 0));
		assert_eq!(extent((250, 48)), (FOOTPRINT.0, 48));
		let saved = Some(Position { x: 2500, y: 6500 });
		assert_eq!(drag((200, 48), saved, (70, 80)), (saved, (0, 0)));
		assert_eq!(drag((200, 720), saved, (50, 208)).0.unwrap().x, 2500);
	}

	#[test]
	fn drag_and_restore_at_different_logical_sizes_use_maximum_footprint() {
		let (saved, clamped) = drag((948, 704), None, (999, -70));
		assert_eq!(clamped, (700, 0));
		assert_eq!(saved, Some(Position { x: 10_000, y: 0 }));
		assert_eq!(offset((748, 804), saved), (500, 0));
		let (saved, restored) = drag((948, 704), saved, (175, 133));
		assert_eq!(restored, (175, 133));
		assert_eq!(offset((1248, 904), saved), (250, 200));
	}
}
