/// Stable hash of a serializable value. Sorts JSON object keys recursively,
/// then streams the canonical JSON bytes into seahash.
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

    fn canonicalize(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Array(values) => values.iter_mut().for_each(canonicalize),
            serde_json::Value::Object(values) => {
                values.values_mut().for_each(canonicalize);
                values.sort_keys();
            }
            _ => {}
        }
    }

    let mut value = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    canonicalize(&mut value);
    let mut writer = HashWriter(seahash::SeaHasher::new());
    if serde_json::to_writer(&mut writer, &value).is_err() {
        return seahash::hash(&[]);
    }
    std::hash::Hasher::finish(&writer.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_serializable_matches_canonical_json_hash() {
        let value = serde_json::json!({
            "z": [3, 2, 1],
            "a": { "nested": true },
        });
        let canonical = br#"{"a":{"nested":true},"z":[3,2,1]}"#;

        assert_eq!(hash_serializable(&value), seahash::hash(canonical));
    }

    #[test]
    fn hash_serializable_ignores_json_object_order_recursively() {
        let first: serde_json::Value =
            serde_json::from_str(r#"{"z":{"second":2,"first":1},"a":{"nested":true}}"#).unwrap();
        let second: serde_json::Value =
            serde_json::from_str(r#"{"a":{"nested":true},"z":{"first":1,"second":2}}"#).unwrap();

        assert_eq!(hash_serializable(&first), hash_serializable(&second));
    }
}
