//! Viewport geometry primitive shared by `grid` and `layout`.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub top: u16,
    pub left: u16,
    pub width: u16,
    pub height: u16,
}

impl Rect {
    pub fn new(top: u16, left: u16, width: u16, height: u16) -> Self {
        Self {
            top,
            left,
            width,
            height,
        }
    }

    pub fn bottom(&self) -> u16 {
        self.top + self.height
    }

    pub fn right(&self) -> u16 {
        self.left + self.width
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        row >= self.top && row < self.bottom() && col >= self.left && col < self.right()
    }

    pub fn area(&self) -> u32 {
        self.width as u32 * self.height as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottom_is_top_plus_height() {
        let r = Rect::new(3, 0, 10, 7);
        assert_eq!(r.bottom(), 10);
    }

    #[test]
    fn right_is_left_plus_width() {
        let r = Rect::new(0, 5, 12, 4);
        assert_eq!(r.right(), 17);
    }

    #[test]
    fn area_multiplies_width_by_height() {
        assert_eq!(Rect::new(0, 0, 80, 24).area(), 1920);
        assert_eq!(Rect::new(0, 0, 0, 99).area(), 0);
    }

    #[test]
    fn contains_includes_top_left_excludes_bottom_right() {
        // Half-open ranges: [top, bottom) × [left, right). The contains
        // signature is (row, col), not (x, y).
        let r = Rect::new(5, 10, 4, 3); // rows 5..8, cols 10..14
        assert!(r.contains(5, 10), "top-left corner should be inside");
        assert!(r.contains(7, 13), "last inclusive cell should be inside");
        assert!(!r.contains(8, 10), "bottom edge is exclusive");
        assert!(!r.contains(5, 14), "right edge is exclusive");
    }

    #[test]
    fn zero_sized_rect_contains_nothing() {
        let r = Rect::new(5, 5, 0, 0);
        assert!(!r.contains(5, 5));
    }
}
