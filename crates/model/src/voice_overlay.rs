use serde::{Deserialize, Serialize};

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
