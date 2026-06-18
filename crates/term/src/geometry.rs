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
        self.top.saturating_add(self.height)
    }

    pub fn right(&self) -> u16 {
        self.left.saturating_add(self.width)
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        row >= self.top && row < self.bottom() && col >= self.left && col < self.right()
    }

    pub fn contains_xy(&self, x: u16, y: u16) -> bool {
        self.contains(y, x)
    }

    pub fn area(&self) -> u32 {
        self.width as u32 * self.height as u32
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn inset(self, amount: u16) -> Rect {
        self.inset_by(Insets::uniform(amount))
    }

    pub fn inset_by(self, insets: Insets) -> Rect {
        let dx = insets.left.saturating_add(insets.right);
        let dy = insets.top.saturating_add(insets.bottom);
        Rect::new(
            self.top.saturating_add(insets.top),
            self.left.saturating_add(insets.left),
            self.width.saturating_sub(dx),
            self.height.saturating_sub(dy),
        )
    }

    pub fn translate(self, rows: i32, cols: i32) -> Rect {
        Rect::new(
            translate_u16(self.top, rows),
            translate_u16(self.left, cols),
            self.width,
            self.height,
        )
    }

    pub fn to_grid(self, origin: Rect) -> Rect {
        Rect::new(
            origin.top.saturating_add(self.top),
            origin.left.saturating_add(self.left),
            self.width,
            self.height,
        )
    }

    pub fn to_local(self, origin: Rect) -> Rect {
        Rect::new(
            self.top.saturating_sub(origin.top),
            self.left.saturating_sub(origin.left),
            self.width,
            self.height,
        )
    }

    pub fn intersection(self, other: Rect) -> Rect {
        let top = self.top.max(other.top);
        let left = self.left.max(other.left);
        let bottom = self.bottom().min(other.bottom());
        let right = self.right().min(other.right());
        Rect::new(
            top.min(bottom),
            left.min(right),
            right.saturating_sub(left),
            bottom.saturating_sub(top),
        )
    }

    pub fn clip_to(self, bounds: Rect) -> Rect {
        self.intersection(bounds)
    }

    pub fn split_top(self, height: u16) -> (Rect, Rect) {
        let top_height = height.min(self.height);
        let top = Rect::new(self.top, self.left, self.width, top_height);
        let rest = Rect::new(
            self.top.saturating_add(top_height),
            self.left,
            self.width,
            self.height.saturating_sub(top_height),
        );
        (top, rest)
    }

    pub fn split_bottom(self, height: u16) -> (Rect, Rect) {
        let bottom_height = height.min(self.height);
        let rest_height = self.height.saturating_sub(bottom_height);
        let rest = Rect::new(self.top, self.left, self.width, rest_height);
        let bottom = Rect::new(
            self.top.saturating_add(rest_height),
            self.left,
            self.width,
            bottom_height,
        );
        (rest, bottom)
    }

    pub fn split_left(self, width: u16) -> (Rect, Rect) {
        let left_width = width.min(self.width);
        let left = Rect::new(self.top, self.left, left_width, self.height);
        let rest = Rect::new(
            self.top,
            self.left.saturating_add(left_width),
            self.width.saturating_sub(left_width),
            self.height,
        );
        (left, rest)
    }

    pub fn split_right(self, width: u16) -> (Rect, Rect) {
        let right_width = width.min(self.width);
        let rest_width = self.width.saturating_sub(right_width);
        let rest = Rect::new(self.top, self.left, rest_width, self.height);
        let right = Rect::new(
            self.top,
            self.left.saturating_add(rest_width),
            right_width,
            self.height,
        );
        (rest, right)
    }

    pub fn split_y(self, top_height: u16, gap: u16) -> (Rect, Rect) {
        let top_height = top_height.min(self.height);
        let bottom_top = self.top.saturating_add(top_height).saturating_add(gap);
        let used = top_height.saturating_add(gap).min(self.height);
        let top = Rect::new(self.top, self.left, self.width, top_height);
        let bottom = Rect::new(
            bottom_top.min(self.bottom()),
            self.left,
            self.width,
            self.height.saturating_sub(used),
        );
        (top, bottom)
    }

    pub fn split_x(self, left_width: u16, gap: u16) -> (Rect, Rect) {
        let left_width = left_width.min(self.width);
        let right_left = self.left.saturating_add(left_width).saturating_add(gap);
        let used = left_width.saturating_add(gap).min(self.width);
        let left = Rect::new(self.top, self.left, left_width, self.height);
        let right = Rect::new(
            self.top,
            right_left.min(self.right()),
            self.width.saturating_sub(used),
            self.height,
        );
        (left, right)
    }

    pub fn centered(self, width: u16, height: u16) -> Rect {
        let width = width.min(self.width);
        let height = height.min(self.height);
        Rect::new(
            self.top.saturating_add((self.height - height) / 2),
            self.left.saturating_add((self.width - width) / 2),
            width,
            height,
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Insets {
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
    pub left: u16,
}

impl Insets {
    pub fn new(top: u16, right: u16, bottom: u16, left: u16) -> Self {
        Self {
            top,
            right,
            bottom,
            left,
        }
    }

    pub fn uniform(amount: u16) -> Self {
        Self::new(amount, amount, amount, amount)
    }
}

fn translate_u16(value: u16, delta: i32) -> u16 {
    if delta.is_negative() {
        value.saturating_sub(delta.unsigned_abs().min(u16::MAX as u32) as u16)
    } else {
        value.saturating_add(delta.min(u16::MAX as i32) as u16)
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

    #[test]
    fn inset_clamps_tiny_rects() {
        assert_eq!(Rect::new(1, 2, 3, 2).inset(2), Rect::new(3, 4, 0, 0));
        assert_eq!(
            Rect::new(0, 0, 10, 8).inset_by(Insets::new(1, 2, 3, 4)),
            Rect::new(1, 4, 4, 4)
        );
    }

    #[test]
    fn splits_with_gap_do_not_underflow() {
        let r = Rect::new(2, 3, 5, 4);
        assert_eq!(
            r.split_y(3, 10),
            (Rect::new(2, 3, 5, 3), Rect::new(6, 3, 5, 0))
        );
        assert_eq!(
            r.split_x(4, 10),
            (Rect::new(2, 3, 4, 4), Rect::new(2, 8, 0, 4))
        );
    }

    #[test]
    fn centered_clamps_oversized_child() {
        let r = Rect::new(10, 20, 6, 4);
        assert_eq!(r.centered(2, 2), Rect::new(11, 22, 2, 2));
        assert_eq!(r.centered(99, 99), r);
        assert_eq!(
            Rect::new(u16::MAX - 1, u16::MAX - 1, 4, 4)
                .centered(2, 2)
                .top,
            u16::MAX
        );
    }

    #[test]
    fn intersection_returns_overlap_or_empty_rect() {
        assert_eq!(
            Rect::new(2, 3, 10, 8).intersection(Rect::new(5, 1, 6, 4)),
            Rect::new(5, 3, 4, 4)
        );
        assert_eq!(
            Rect::new(0, 0, 2, 2).intersection(Rect::new(5, 5, 2, 2)),
            Rect::new(2, 2, 0, 0)
        );
    }

    #[test]
    fn translate_saturates_at_zero() {
        assert_eq!(
            Rect::new(2, 3, 4, 5).translate(-10, -1),
            Rect::new(0, 2, 4, 5)
        );
        assert_eq!(
            Rect::new(60_000, 3, 4, 5).translate(-60_000, 4),
            Rect::new(0, 7, 4, 5)
        );
        assert_eq!(Rect::new(2, 3, 4, 5).translate(2, 4), Rect::new(4, 7, 4, 5));
    }

    #[test]
    fn grid_and_local_convert_coordinate_spaces() {
        let origin = Rect::new(10, 20, 30, 40);
        let local = Rect::new(2, 3, 4, 5);
        let grid = Rect::new(12, 23, 4, 5);
        assert_eq!(local.to_grid(origin), grid);
        assert_eq!(grid.to_local(origin), local);
    }
}
