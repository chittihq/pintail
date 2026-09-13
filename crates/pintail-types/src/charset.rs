//! Character encodings at SQL's text/byte boundary. Execution stores Unicode;
//! a character set determines the bytes observed by hashes and byte functions.

/// Encodings with an explicit, loss-aware Unicode conversion.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum CharacterSet {
    /// Unicode encoded as UTF-8.
    #[default]
    Utf8Mb4,
    /// UTF-8 restricted to the basic multilingual plane.
    Utf8Mb3,
    /// Big-endian basic-plane code units.
    Ucs2,
    /// Big-endian UTF-16, including surrogate pairs.
    Utf16,
    /// Little-endian UTF-16.
    Utf16Le,
    /// Big-endian Unicode scalar values.
    Utf32,
}

impl CharacterSet {
    /// Resolve a SQL character-set name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "utf8mb4" => Some(Self::Utf8Mb4),
            "utf8" | "utf8mb3" => Some(Self::Utf8Mb3),
            "ucs2" => Some(Self::Ucs2),
            "utf16" => Some(Self::Utf16),
            "utf16le" => Some(Self::Utf16Le),
            "utf32" => Some(Self::Utf32),
            _ => None,
        }
    }

    /// Default comparison name selected by a connection character set.
    #[must_use]
    pub const fn default_collation(self) -> &'static str {
        match self {
            Self::Utf8Mb4 => "utf8mb4_0900_ai_ci",
            Self::Utf8Mb3 => "utf8mb3_general_ci",
            Self::Ucs2 => "ucs2_general_ci",
            Self::Utf16 => "utf16_general_ci",
            Self::Utf16Le => "utf16le_general_ci",
            Self::Utf32 => "utf32_general_ci",
        }
    }

    /// Minimum encoded character width, including introducer padding.
    #[must_use]
    pub const fn minimum_width(self) -> usize {
        match self {
            Self::Utf8Mb4 | Self::Utf8Mb3 => 1,
            Self::Ucs2 | Self::Utf16 | Self::Utf16Le => 2,
            Self::Utf32 => 4,
        }
    }

    /// Encode Unicode, replacing characters outside the target repertoire.
    #[must_use]
    pub fn encode(self, text: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(text.len());
        for mut character in text.chars() {
            if matches!(self, Self::Ucs2 | Self::Utf8Mb3) && u32::from(character) > 0xffff {
                character = '?';
            }
            match self {
                Self::Utf8Mb4 | Self::Utf8Mb3 => {
                    bytes.extend_from_slice(character.encode_utf8(&mut [0; 4]).as_bytes());
                }
                Self::Ucs2 | Self::Utf16 | Self::Utf16Le => {
                    for unit in character.encode_utf16(&mut [0; 2]) {
                        bytes.extend_from_slice(&if self == Self::Utf16Le {
                            unit.to_le_bytes()
                        } else {
                            unit.to_be_bytes()
                        });
                    }
                }
                Self::Utf32 => bytes.extend_from_slice(&u32::from(character).to_be_bytes()),
            }
        }
        bytes
    }

    /// Decode valid encoded text. Ill-formed input has no Unicode carrier.
    #[must_use]
    pub fn decode(self, bytes: &[u8]) -> Option<String> {
        match self {
            Self::Utf8Mb4 | Self::Utf8Mb3 => {
                let text = std::str::from_utf8(bytes).ok()?;
                if self == Self::Utf8Mb3 && text.chars().any(|c| u32::from(c) > 0xffff) {
                    return None;
                }
                Some(text.to_owned())
            }
            Self::Ucs2 | Self::Utf16 | Self::Utf16Le => {
                if !bytes.len().is_multiple_of(2) {
                    return None;
                }
                let units = bytes.chunks_exact(2).map(|pair| {
                    if self == Self::Utf16Le {
                        u16::from_le_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_be_bytes([pair[0], pair[1]])
                    }
                });
                if self == Self::Ucs2 {
                    units.map(|unit| char::from_u32(u32::from(unit))).collect()
                } else {
                    char::decode_utf16(units)
                        .collect::<Result<String, _>>()
                        .ok()
                }
            }
            Self::Utf32 => {
                if !bytes.len().is_multiple_of(4) {
                    return None;
                }
                bytes
                    .chunks_exact(4)
                    .map(|part| {
                        char::from_u32(u32::from_be_bytes([part[0], part[1], part[2], part[3]]))
                    })
                    .collect()
            }
        }
    }
}
