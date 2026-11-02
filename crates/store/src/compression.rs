pub const DEFAULT_ZSTD_LEVEL: i32 = 1;
pub const DEFAULT_ZSTD_MIN_BYTES: usize = 4 * 1024;
pub const DEFAULT_ZSTD_MIN_SAVINGS_PERCENT: u8 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectCompression {
    None,
    Zstd {
        level: i32,
        min_bytes: usize,
        min_savings_percent: u8,
    },
}

impl ObjectCompression {
    pub const fn none() -> Self {
        Self::None
    }

    pub const fn zstd(level: i32, min_bytes: usize, min_savings_percent: u8) -> Self {
        Self::Zstd {
            level,
            min_bytes,
            min_savings_percent,
        }
    }

    pub(crate) fn zstd_settings(self) -> Option<(i32, usize, u8)> {
        match self {
            ObjectCompression::None => None,
            ObjectCompression::Zstd {
                level,
                min_bytes,
                min_savings_percent,
            } => Some((level, min_bytes, min_savings_percent)),
        }
    }
}

impl Default for ObjectCompression {
    fn default() -> Self {
        Self::zstd(
            DEFAULT_ZSTD_LEVEL,
            DEFAULT_ZSTD_MIN_BYTES,
            DEFAULT_ZSTD_MIN_SAVINGS_PERCENT,
        )
    }
}

pub(crate) fn accepts_compressed_size(
    raw_size: usize,
    stored_size: usize,
    min_savings_percent: u8,
) -> bool {
    if stored_size >= raw_size {
        return false;
    }
    let saved = raw_size - stored_size;
    saved * 100 >= raw_size * usize::from(min_savings_percent)
}
