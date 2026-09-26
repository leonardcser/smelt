//! Shared raster assets. Retained display spans and painted cells own these
//! handles so an image lives exactly as long as its cached or visible uses.

use std::{fmt, sync::Arc};

#[derive(Eq)]
pub struct RasterImage {
    pub id: u32,
    pub png_base64: Arc<str>,
    pub cols: u16,
    pub rows: u16,
}

// Image IDs uniquely identify assets; comparing PNG payloads during frame diffing is costly.
impl PartialEq for RasterImage {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl fmt::Debug for RasterImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RasterImage")
            .field("id", &self.id)
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .finish_non_exhaustive()
    }
}
