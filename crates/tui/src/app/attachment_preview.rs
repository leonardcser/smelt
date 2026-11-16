//! A picture of the prompt attachment under the cursor, on top of
//! everything else right above the label, from the label's column. It opens
//! when the cursor reaches a label and closes when the cursor leaves it.
//! Pictures are built when an image is attached, so the first visit does not
//! stall a frame.

use std::sync::Arc;

use smelt_buffer::attachment::Attachment;
use smelt_view::picture::{self, Align, Picture};

use crate::app::TuiApp;
use crate::smelt_edit::{
    Anchor, BufId, Constraint, Corner, Gutters, LayoutTree, Overlay, OverlayId, SplitConfig, WinId,
    WindowSurface,
};

/// The cell box every preview fits in. It is fixed so the picture built at
/// attach time is the one the cursor finds.
const COLS: u16 = 48;
const ROWS: u16 = 12;

/// The preview picture of an attachment, from the picture cache or built now.
fn preview_picture(attachment: &Attachment) -> Option<Arc<Picture>> {
    picture::fitted(attachment.content_key(), COLS, ROWS, Align::Bottom, || {
        engine::image::decode_data_url(attachment.data_url())
    })
}

/// Build an attachment's preview on a background thread.
pub(crate) fn prewarm(attachment: Attachment) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        preview_picture(&attachment);
    })
}

/// The open preview, the picture it shows and where.
pub(crate) struct AttachmentPreview {
    overlay: OverlayId,
    picture: Arc<Picture>,
    anchor: Anchor,
}

impl TuiApp {
    /// Show the attachment under the prompt cursor, or close the preview.
    pub(crate) fn sync_attachment_preview(&mut self, has_prompt_cursor: bool) {
        let wanted = has_prompt_cursor
            .then(|| self.preview_under_prompt_cursor())
            .flatten();
        if let (Some(shown), Some((picture, anchor))) =
            (self.overlays.attachment_preview(), &wanted)
        {
            if Arc::ptr_eq(&shown.picture, picture) && shown.anchor == *anchor {
                return;
            }
        }
        if let Some(shown) = self.overlays.take_attachment_preview() {
            self.close_overlay(shown.overlay);
        }
        if let Some((picture, anchor)) = wanted {
            if let Some(overlay) = self.open_attachment_preview(&picture, anchor.clone()) {
                self.overlays.install_attachment_preview(AttachmentPreview {
                    overlay,
                    picture,
                    anchor,
                });
            }
        }
    }

    /// The picture of the attachment under the prompt cursor and where it
    /// goes: its bottom row right above the label, from the label's column.
    /// There is none when the picture does not fit above the label.
    fn preview_under_prompt_cursor(&self) -> Option<(Arc<Picture>, Anchor)> {
        let win = self.prompt_win();
        let buf = self.prompt_buf();
        // The painted caret: a mouse press moves it before release sets `cpos`.
        let picture = preview_picture(buf.attachment_at(win.effective_endpoint())?)?;
        let rect = self.ui.split_rect(crate::app::PROMPT_WIN)?;
        let (row, col) = win.caret_position(buf, rect.width, rect.height)?;
        let label_row = rect.y + row;
        let (term_w, _) = self.ui.terminal_size();
        if picture.cols() > term_w || picture.rows().len() > usize::from(label_row) {
            return None;
        }
        let anchor = Anchor::ScreenAt {
            row: i32::from(label_row) - 1,
            col: i32::from(rect.x + col),
            corner: Corner::SW,
        };
        Some((picture, anchor))
    }

    fn open_attachment_preview(&mut self, picture: &Picture, anchor: Anchor) -> Option<OverlayId> {
        let buf: BufId = self
            .ui
            .buf_create(crate::smelt_edit::BufCreateOpts::default());
        let b = self.ui.buf_mut(buf)?;
        b.readonly = true;
        picture.write_to(b);
        let win: WinId = self.ui.win_open_split(
            buf,
            SplitConfig {
                gutters: Gutters {
                    scrollbar: false,
                    ..Gutters::default()
                },
            },
        )?;
        self.ui.win_mut(win)?.set_surface(WindowSurface::inert());
        let layout = LayoutTree::vbox(vec![(
            Constraint::Length(picture.rows().len() as u16),
            LayoutTree::hbox(vec![(
                Constraint::Length(picture.cols()),
                LayoutTree::leaf(win),
            )]),
        )]);
        let overlay =
            Overlay::new(layout, anchor).with_z(crate::content::layout::z::ATTACHMENT_PREVIEW);
        Some(self.ui.overlay_open(overlay))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prewarmed_previews_are_cache_hits() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(64, 32, image::Rgba([0, 128, 255, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let data_url = engine::image::data_url_from_bytes(&png, "image/png");
        let attachment = Attachment::image("blue.png".into(), data_url.clone());
        prewarm(attachment).join().unwrap();
        // Same content, so the same cache key: a hit never decodes this one.
        let twin = Attachment::image("blue.png".into(), data_url);
        let picture = picture::fitted(twin.content_key(), COLS, ROWS, Align::Bottom, || None)
            .expect("cached picture");
        assert_eq!((picture.cols(), picture.rows().len()), (8, 2));
    }
}
