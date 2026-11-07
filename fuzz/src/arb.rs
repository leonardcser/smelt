//! Small `arbitrary` generators shared by the fuzz targets.

use arbitrary::Unstructured;

/// Up to `max` arbitrary bytes.
pub fn short_bytes(u: &mut Unstructured<'_>, max: usize) -> arbitrary::Result<Vec<u8>> {
    let len = u.int_in_range(0..=max)?;
    (0..len).map(|_| u.arbitrary::<u8>()).collect()
}

/// Up to `max` arbitrary bytes, lossily decoded as UTF-8.
pub fn short_string(u: &mut Unstructured<'_>, max: usize) -> arbitrary::Result<String> {
    Ok(String::from_utf8_lossy(&short_bytes(u, max)?).into_owned())
}
