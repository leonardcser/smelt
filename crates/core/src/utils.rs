/// Stable hash of a serializable value. Serializes through `serde_json::Value`
/// first so map keys are sorted, then streams JSON bytes into seahash.
pub fn hash_serializable<T: serde::Serialize>(value: &T) -> u64 {
    struct HashWriter(seahash::SeaHasher);

    impl std::io::Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            std::hash::Hasher::write(&mut self.0, buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    let mut writer = HashWriter(seahash::SeaHasher::new());
    if serde_json::to_writer(&mut writer, &value).is_err() {
        return seahash::hash(&[]);
    }
    std::hash::Hasher::finish(&writer.0)
}

pub fn format_duration(secs: u64) -> String {
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

/// Map `f` over `items` in parallel worker threads, dropping `None` results.
/// Output order is not stable.
pub fn parallel_filter_map<T, R, F>(items: Vec<T>, f: F) -> Vec<R>
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
    fn hash_serializable_matches_buffered_json_hash() {
        let value = serde_json::json!({
            "z": [3, 2, 1],
            "a": { "nested": true },
        });
        let json = serde_json::to_value(&value).unwrap();
        let bytes = serde_json::to_vec(&json).unwrap();

        assert_eq!(hash_serializable(&value), seahash::hash(&bytes));
    }

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
