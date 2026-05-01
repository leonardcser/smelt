/// Format elapsed seconds into a human-readable string.
///
/// Format rules:
/// - 0-59s: "Xs" (e.g., "45s")
/// - 1-59m: "Xm Ys" (e.g., "3m 27s")
/// - 1h+: "Xh Ym Zs" (e.g., "2h 15m 30s")
pub(crate) fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let minutes = secs / 60;
        let remaining_secs = secs % 60;
        format!("{minutes}m {remaining_secs}s")
    } else {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let remaining_secs = secs % 60;
        format!("{hours}h {minutes}m {remaining_secs}s")
    }
}

/// Map `f` over `items` across `available_parallelism()` worker threads,
/// dropping `None` results. Output order is not stable across threads.
pub(crate) fn parallel_filter_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Option<R> + Send + Sync + Clone + 'static,
{
    if items.is_empty() {
        return Vec::new();
    }
    let n_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(items.len())
        .max(1);
    let chunk_size = items.len().div_ceil(n_workers).max(1);

    let mut remaining = items;
    let mut handles = Vec::with_capacity(n_workers);
    while !remaining.is_empty() {
        let take = chunk_size.min(remaining.len());
        let chunk: Vec<T> = remaining.drain(..take).collect();
        let f = f.clone();
        handles.push(std::thread::spawn(move || -> Vec<R> {
            chunk.into_iter().filter_map(&f).collect()
        }));
    }

    let mut out = Vec::with_capacity(handles.len() * chunk_size);
    for h in handles {
        if let Ok(part) = h.join() {
            out.extend(part);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_seconds_only() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(1), "1s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn formats_minutes_and_seconds() {
        assert_eq!(format_duration(60), "1m 0s");
        assert_eq!(format_duration(61), "1m 1s");
        assert_eq!(format_duration(127), "2m 7s");
        assert_eq!(format_duration(3599), "59m 59s");
    }

    #[test]
    fn formats_hours_minutes_and_seconds() {
        assert_eq!(format_duration(3600), "1h 0m 0s");
        assert_eq!(format_duration(3601), "1h 0m 1s");
        assert_eq!(format_duration(7267), "2h 1m 7s");
        assert_eq!(format_duration(5430), "1h 30m 30s");
    }
}
