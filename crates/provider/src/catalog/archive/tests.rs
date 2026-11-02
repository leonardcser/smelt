use super::*;
use std::io::Cursor;

fn packed(json: &str) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    write(json.as_bytes(), &mut output, 0).unwrap();
    output.into_inner()
}

#[test]
fn roundtrip_preserves_aliases_collisions_and_key_precedence() {
    let json = r#"{
        "b": {"api":"https://shared.example/v1", "models":{
            "Exact Name":{"limit":{"output":400}},
            "exact-name":{"limit":{"output":200}},
            "unique":{"reasoning":false}
        }},
        "a": {"name":"Provider A", "api":"https://SHARED.example/v1/", "models":{
            "a":{"name":"Exact Name", "last_updated":"2025-01", "limit":{"output":100}},
            "z":{"name":"Exact Name", "last_updated":"2026-01", "limit":{"output":300}}
        }}
    }"#;
    let expected = super::super::parse(json).unwrap();
    let archive = Archive::open(Cursor::new(packed(json))).unwrap();
    for (provider, models) in &expected.providers {
        for (model, index) in models {
            assert_eq!(
                archive
                    .lookup(std::slice::from_ref(provider), model)
                    .unwrap()
                    .as_ref(),
                Some(&expected.entries[*index])
            );
        }
    }
    // Exact model IDs win across all providers sharing an API before the
    // slug fallback is considered. Input provider order is not precedence.
    assert_eq!(
        archive
            .lookup(&["api:https://shared.example/v1".into()], "Exact Name")
            .unwrap()
            .unwrap()
            .output_tokens,
        Some(400)
    );
    assert_eq!(
        archive
            .lookup(&["b".into(), "a".into()], "exact-name")
            .unwrap()
            .unwrap()
            .output_tokens,
        Some(200)
    );
    assert!(archive
        .lookup(&["unknown".into()], "exact-name")
        .unwrap()
        .is_none());
}

fn raw_archive(providers: &[(&str, &str)]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut blocks = Vec::new();
    for (id, json) in providers {
        let offset = bytes.len() as u64;
        let compressed = zstd::encode_all(json.as_bytes(), 3).unwrap();
        bytes.extend_from_slice(&compressed);
        blocks.push(ProviderBlock {
            id: (*id).into(),
            api: None,
            offset,
            len: compressed.len() as u64,
        });
    }
    let start = bytes.len();
    serde_json::to_writer(
        &mut bytes,
        &Directory {
            expires: 0,
            providers: blocks,
        },
    )
    .unwrap();
    let len = (bytes.len() - start) as u64;
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(MAGIC);
    bytes
}

#[test]
fn lookup_deserializes_only_the_requested_model() {
    let bytes = raw_archive(&[(
        "p",
        r#"{
        "requested":{"context_window":123},
        "unrequested":{"pricing":"invalid ModelEntry, valid JSON"}
    }"#,
    )]);
    let archive = Archive::open(Cursor::new(bytes)).unwrap();
    assert_eq!(
        archive
            .lookup(&["p".into()], "requested")
            .unwrap()
            .unwrap()
            .context_window,
        Some(123)
    );
    assert!(archive.lookup(&["p".into()], "unrequested").is_err());
}

#[test]
fn opening_and_other_provider_lookups_do_not_decode_unrequested_blocks() {
    let mut bytes = raw_archive(&[
        ("a", r#"{"m":{"context_window":1}}"#),
        ("b", r#"{"m":{"context_window":2}}"#),
    ]);
    bytes[0] ^= 0xff;
    let archive = Archive::open(Cursor::new(bytes)).unwrap();
    assert_eq!(
        archive
            .lookup(&["b".into()], "m")
            .unwrap()
            .unwrap()
            .context_window,
        Some(2)
    );
    assert!(archive.lookup(&["a".into()], "m").is_err());
}

#[test]
fn malformed_directories_are_rejected_without_panics() {
    let bytes = packed(r#"{"p":{"models":{"m":{"reasoning":true}}}}"#);
    for end in 0..bytes.len() {
        assert!(Archive::open(Cursor::new(bytes[..end].to_vec())).is_err());
    }
    for len in [u64::MAX, MAX_DIRECTORY_LEN + 1] {
        let mut corrupt = bytes.clone();
        let end = corrupt.len() - FOOTER_LEN as usize;
        corrupt[end..end + 8].copy_from_slice(&len.to_le_bytes());
        assert!(Archive::open(Cursor::new(corrupt)).is_err());
    }
    let mut corrupt = bytes;
    *corrupt.last_mut().unwrap() ^= 0xff;
    assert!(Archive::open(Cursor::new(corrupt)).is_err());
}

#[test]
fn archive_compilation_is_deterministic_and_rejects_duplicate_providers() {
    let a = r#"{"b":{"models":{"m":{"reasoning":true}}},"a":{"models":{"m":{"reasoning":false}}}}"#;
    assert_eq!(packed(a), packed(a));
    assert!(super::super::write_snapshot(
        r#"{"p":{"models":{"m":{"reasoning":true}}},"p":{"models":{"m":{"reasoning":false}}}}"#
    )
    .is_err());
}
