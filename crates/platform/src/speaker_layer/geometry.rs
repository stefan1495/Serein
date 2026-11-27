use model::voice_overlay::{LayoutPanel, LayoutPositions, LayoutStatus, Position, ToastCorner};

pub(super) const FOOTPRINT: (i32, i32) = (super::WIDTH, super::ROW * super::MAX_ROWS as i32);
const INSET: i32 = 12;
const DENOMINATOR: i64 = Position::DENOMINATOR as i64;

pub(super) fn extent(size: (i32, i32)) -> (i32, i32) {
	(size.0.min(FOOTPRINT.0), size.1.min(FOOTPRINT.1))
}

pub(super) const fn footprint(panel: LayoutPanel) -> (i32, i32) {
	match panel {
		LayoutPanel::Voice => FOOTPRINT,
		LayoutPanel::Notification => super::toast::SIZE,
	}
}

fn travel(size: (i32, i32), panel: LayoutPanel) -> (i32, i32) {
	let extent = footprint(panel);
	((size.0 - extent.0).max(0), (size.1 - extent.1).max(0))
}

pub(super) fn offset(size: (i32, i32), position: Option<Position>) -> (i32, i32) {
	let travel = travel(size, LayoutPanel::Voice);
	position.map_or((travel.0.min(INSET), travel.1.min(INSET)), |p| {
		normalized_offset(travel, p)
	})
}

fn normalized_offset(travel: (i32, i32), position: Position) -> (i32, i32) {
	fn axis(travel: i32, fraction: u16) -> i32 {
		((i64::from(travel) * i64::from(fraction.min(Position::DENOMINATOR)) + DENOMINATOR / 2)
			/ DENOMINATOR) as i32
	}
	(axis(travel.0, position.x), axis(travel.1, position.y))
}

pub(super) fn notification_offset(
	size: (i32, i32),
	corner: ToastCorner,
	position: Option<Position>,
) -> Option<(i32, i32)> {
	let extent = footprint(LayoutPanel::Notification);
	if size.0 < extent.0 || size.1 < extent.1 {
		return None;
	}
	let (x, y) = travel(size, LayoutPanel::Notification);
	if let Some(position) = position {
		return position
			.is_valid()
			.then(|| normalized_offset((x, y), position));
	}
	if x < 2 * INSET || y < 2 * INSET {
		return None;
	}
	Some(corner_offset((x, y), corner))
}

fn corner_offset((x, y): (i32, i32), corner: ToastCorner) -> (i32, i32) {
	match corner {
		ToastCorner::TopLeft => (INSET, INSET),
		ToastCorner::TopRight => (x - INSET, INSET),
		ToastCorner::BottomLeft => (INSET, y - INSET),
		ToastCorner::BottomRight => (x - INSET, y - INSET),
	}
}

pub(super) fn editor_notification_position(
	size: (i32, i32),
	corner: ToastCorner,
) -> Option<Position> {
	let panel = LayoutPanel::Notification;
	let (w, h) = footprint(panel);
	if size.0 < w || size.1 < h {
		return None;
	}
	// The editor can place the full toast flush against an edge; live legacy mode requires insets.
	drag(
		size,
		panel,
		Some(Position { x: 0, y: 0 }),
		corner_offset(travel(size, panel), corner),
	)
}

pub(super) fn drag(
	size: (i32, i32),
	panel: LayoutPanel,
	previous: Option<Position>,
	wanted: (i32, i32),
) -> Option<Position> {
	fn axis(travel: i32, wanted: i32, previous: Option<u16>) -> u16 {
		if travel == 0 {
			return previous.unwrap_or(0);
		}
		let wanted = i64::from(wanted.clamp(0, travel));
		((wanted * DENOMINATOR + i64::from(travel) / 2) / i64::from(travel)) as u16
	}
	let (x, y) = travel(size, panel);
	if (x, y) == (0, 0) {
		previous
	} else {
		Some(Position {
			x: axis(x, wanted.0, previous.map(|p| p.x)),
			y: axis(y, wanted.1, previous.map(|p| p.y)),
		})
	}
}

pub(super) fn overlap(voice: (i32, i32), notification: (i32, i32)) -> bool {
	let (vx, vy) = voice;
	let (nx, ny) = notification;
	let (vw, vh) = FOOTPRINT;
	let (nw, nh) = footprint(LayoutPanel::Notification);
	nx < vx + vw && nx + nw > vx && ny < vy + vh && ny + nh > vy
}

pub(super) fn can_edit(size: (i32, i32)) -> bool {
	let (vw, vh) = FOOTPRINT;
	let (nw, nh) = footprint(LayoutPanel::Notification);
	size.0 >= vw.max(nw) && size.1 >= vh.max(nh) && (size.0 >= vw + nw || size.1 >= vh + nh)
}

pub(super) fn layout_status(
	size: (i32, i32),
	corner: ToastCorner,
	positions: LayoutPositions,
) -> LayoutStatus {
	if !can_edit(size) {
		return LayoutStatus::TooSmall;
	}
	let Some(notification) = notification_offset(size, corner, positions.notification) else {
		return LayoutStatus::TooSmall;
	};
	if overlap(offset(size, positions.voice), notification) {
		LayoutStatus::Overlap
	} else {
		LayoutStatus::Ready
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn legacy_voice_inset_clamps_and_small_outputs_preserve_saved_fractions() {
		assert_eq!(offset((960, 720), None), (12, 12));
		assert_eq!(offset((250, 48), None), (2, 0));
		let saved = Some(Position { x: 2500, y: 6500 });
		assert_eq!(drag((200, 48), LayoutPanel::Voice, saved, (70, 80)), saved);
		assert_eq!(
			drag((200, 720), LayoutPanel::Voice, saved, (50, 208))
				.unwrap()
				.x,
			2500
		);
	}

	#[test]
	fn cropped_roster_geometry_only_admits_whole_rows() {
		use super::super::{MAX_ROWS, ROW, roster_slots};
		assert_eq!(roster_slots((248, ROW * 2 - 1)), 1);
		assert_eq!(roster_slots((248, ROW - 1)), 0);
		assert_eq!(roster_slots((248, ROW * 9)), MAX_ROWS);
	}

	#[test]
	fn drag_clamps_and_restores_using_each_panels_full_footprint() {
		let size = (948, 704);
		let saved = Some(Position { x: 10_000, y: 0 });
		assert_eq!(drag(size, LayoutPanel::Voice, None, (999, -70)), saved);
		assert_eq!(offset((748, 804), saved), (500, 0));
		let notification = drag(size, LayoutPanel::Notification, None, (-100, 990));
		assert_eq!(
			notification_offset(size, ToastCorner::TopRight, notification),
			Some((0, 600))
		);
		assert_eq!(
			notification_offset((748, 804), ToastCorner::TopRight, notification),
			Some((0, 700))
		);
	}

	#[test]
	fn tight_editor_fits_both_footprints_without_relaxing_unset_live_insets() {
		let size = (320, 408);
		let corner = ToastCorner::default();
		assert_eq!(notification_offset(size, corner, None), None);
		assert_eq!(
			notification_offset(size, corner, editor_notification_position(size, corner)),
			Some((0, 12))
		);
		assert_eq!(
			layout_status(
				size,
				corner,
				LayoutPositions {
					voice: Some(Position { x: 0, y: 0 }),
					notification: Some(Position { x: 0, y: 10_000 }),
				}
			),
			LayoutStatus::Ready
		);
		assert!(!can_edit((320, 407)));
	}

	#[test]
	fn layout_status_uses_full_footprints_and_allows_touching_edges() {
		let positions = LayoutPositions {
			voice: Some(Position { x: 0, y: 0 }),
			notification: Some(Position { x: 10_000, y: 0 }),
		};
		let status = |size, positions| layout_status(size, ToastCorner::TopRight, positions);
		assert_eq!(status((568, 304), positions), LayoutStatus::Ready);
		assert_eq!(status((567, 304), positions), LayoutStatus::TooSmall);
		let notification = drag((568, 304), LayoutPanel::Notification, None, (247, 0));
		assert_eq!(
			status(
				(568, 304),
				LayoutPositions {
					notification,
					..positions
				}
			),
			LayoutStatus::Overlap
		);
	}
}
