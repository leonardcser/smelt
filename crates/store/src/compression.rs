use std::time::{Duration, Instant};

use crate::error::Result;

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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionSample {
    pub raw_size: usize,
    pub zstd_size: usize,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompressionReport {
    pub zstd_level: i32,
    pub samples: Vec<CompressionSample>,
    pub total_raw_size: usize,
    pub total_zstd_size: usize,
    pub elapsed: Duration,
}

impl CompressionReport {
    pub fn compression_ratio_percent(&self) -> u8 {
        if self.total_raw_size == 0 {
            return 100;
        }
        ((self.total_zstd_size * 100) / self.total_raw_size).min(100) as u8
    }

    pub fn savings_percent(&self) -> u8 {
        100 - self.compression_ratio_percent()
    }

    pub fn supports_policy(&self, min_savings_percent: u8) -> bool {
        !self.samples.is_empty() && self.savings_percent() >= min_savings_percent
    }
}

pub fn benchmark_zstd_compression<'a>(
    samples: impl IntoIterator<Item = &'a [u8]>,
    level: i32,
) -> Result<CompressionReport> {
    let mut report = CompressionReport {
        zstd_level: level,
        samples: Vec::new(),
        total_raw_size: 0,
        total_zstd_size: 0,
        elapsed: Duration::ZERO,
    };

    for sample in samples {
        let started = Instant::now();
        let compressed = zstd::bulk::compress(sample, level)?;
        let elapsed = started.elapsed();
        report.total_raw_size += sample.len();
        report.total_zstd_size += compressed.len();
        report.elapsed += elapsed;
        report.samples.push(CompressionSample {
            raw_size: sample.len(),
            zstd_size: compressed.len(),
            elapsed,
        });
    }

    Ok(report)
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
