use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RosterMode {
	#[default]
	SpeakingOnly,
	Everyone,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToastCorner {
	TopLeft,
	#[default]
	TopRight,
	BottomLeft,
	BottomRight,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct NotificationSettings {
	pub enabled: bool,
	pub include_excerpts: bool,
	pub corner: ToastCorner,
	pub position: Option<Position>,
}

/// Bound text sent to the native renderer without splitting UTF-8 or honoring bidi overrides.
pub fn label(text: &str, max_chars: usize, max_bytes: usize) -> String {
	let mut output = String::new();
	for ch in text.chars().take(max_chars) {
		if ch.is_control()
			|| matches!(ch, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
		{
			continue;
		}
		if output.len() + ch.len_utf8() > max_bytes {
			break;
		}
		output.push(ch);
	}
	output.truncate(output.trim_end().len());
	output.drain(..output.len() - output.trim_start().len());
	output
}

/// Position as fractions of available travel, in ten-thousandths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Position {
	pub x: u16,
	pub y: u16,
}

impl Position {
	pub const DENOMINATOR: u16 = 10_000;

	pub fn is_valid(self) -> bool {
		self.x <= Self::DENOMINATOR && self.y <= Self::DENOMINATOR
	}
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayoutPositions {
	pub voice: Option<Position>,
	pub notification: Option<Position>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutPanel {
	Voice,
	Notification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutStatus {
	Ready,
	Overlap,
	TooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutAction {
	Nudge { panel: LayoutPanel, x: i8, y: i8 },
	Reset(LayoutPanel),
	Save { revision: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutCommand {
	pub sequence: u64,
	pub action: LayoutAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutAck {
	pub revision: u64,
	pub command: u64,
	pub positions: LayoutPositions,
	pub dragging: bool,
	pub status: LayoutStatus,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FontWeight {
	Normal,
	#[default]
	Semibold,
	Bold,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Appearance {
	pub show_avatars: bool,
	pub avatar_size: u8,
	pub font_family: Option<String>,
	pub font_size: u8,
	pub font_weight: FontWeight,
	pub text: [u8; 3],
	pub background: [u8; 3],
	pub background_opacity: u8,
	pub accent: [u8; 3],
}

impl Default for Appearance {
	fn default() -> Self {
		Self {
			show_avatars: true,
			avatar_size: 28,
			font_family: None,
			font_size: 11,
			font_weight: FontWeight::Semibold,
			text: [255; 3],
			background: [23, 23, 28],
			background_opacity: 94,
			accent: [51, 204, 120],
		}
	}
}

impl Appearance {
	pub const AVATAR_SIZES: std::ops::RangeInclusive<u8> = 16..=32;
	pub const FONT_SIZES: std::ops::RangeInclusive<u8> = 8..=20;

	pub fn valid_font_family(name: &str) -> bool {
		!name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
	}

	pub fn is_valid(&self) -> bool {
		Self::AVATAR_SIZES.contains(&self.avatar_size)
			&& Self::FONT_SIZES.contains(&self.font_size)
			&& self.background_opacity <= 100
			&& self
				.font_family
				.as_deref()
				.is_none_or(Self::valid_font_family)
	}
}
