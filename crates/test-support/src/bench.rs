//! Statistics shared by isolated performance fixtures, never production code.

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub mean: f64,
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

impl Stats {
    pub fn from(values: &[f64]) -> Self {
        assert!(!values.is_empty(), "benchmark statistics require samples");
        assert!(
            values.iter().all(|value| value.is_finite()),
            "benchmark samples must be finite"
        );
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let variance = if values.len() > 1 {
            values
                .iter()
                .map(|value| (value - mean).powi(2))
                .sum::<f64>()
                / (values.len() - 1) as f64
        } else {
            0.0
        };
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        let percentile = |percent: usize| {
            let rank = sorted.len().saturating_mul(percent).div_ceil(100).max(1);
            sorted[rank - 1]
        };
        Self {
            mean,
            stddev: variance.sqrt(),
            min: sorted[0],
            max: sorted[sorted.len() - 1],
            p50: percentile(50),
            p95: percentile(95),
            p99: percentile(99),
        }
    }

    pub fn display(self) -> String {
        format!("{:.2}±{:.2}", self.mean, self.stddev)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_sample_deviation_and_nearest_rank_percentiles() {
        let stats = Stats::from(&[5.0, 1.0, 3.0, 2.0, 4.0]);
        assert_eq!((stats.mean, stats.min, stats.max), (3.0, 1.0, 5.0));
        assert_eq!(stats.stddev, 2.5_f64.sqrt());
        assert_eq!((stats.p50, stats.p95, stats.p99), (3.0, 5.0, 5.0));
        let single = Stats::from(&[7.0]);
        assert_eq!(single.stddev, 0.0);
        assert_eq!((single.min, single.max, single.p50), (7.0, 7.0, 7.0));
    }
}
