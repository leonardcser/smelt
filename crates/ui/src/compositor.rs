use crate::component::{Component, DrawContext, KeyResult};
use crate::flush::flush_diff;
use crate::grid::Grid;
use crate::layout::Rect;
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::QueueableCommand;
use std::io::Write;

struct Layer {
    id: String,
    component: Box<dyn Component>,
    rect: Rect,
    zindex: u16,
}

pub struct Compositor {
    current: Grid,
    previous: Grid,
    layers: Vec<Layer>,
    focused: Option<String>,
    width: u16,
    height: u16,
    force_redraw: bool,
}

impl Compositor {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            current: Grid::new(width, height),
            previous: Grid::new(width, height),
            layers: Vec::new(),
            focused: None,
            width,
            height,
            force_redraw: true,
        }
    }

    pub fn add(
        &mut self,
        id: impl Into<String>,
        component: Box<dyn Component>,
        rect: Rect,
        zindex: u16,
    ) {
        let id = id.into();
        self.layers.push(Layer {
            id,
            component,
            rect,
            zindex,
        });
        self.sort_layers();
    }

    pub fn remove(&mut self, id: &str) -> Option<Box<dyn Component>> {
        if let Some(pos) = self.layers.iter().position(|l| l.id == id) {
            let layer = self.layers.remove(pos);
            if self.focused.as_deref() == Some(id) {
                self.focused = None;
            }
            self.force_redraw = true;
            Some(layer.component)
        } else {
            None
        }
    }

    pub fn set_rect(&mut self, id: &str, rect: Rect) {
        if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
            if layer.rect != rect {
                layer.rect = rect;
                layer.component.mark_dirty();
            }
        }
    }

    pub fn set_zindex(&mut self, id: &str, zindex: u16) {
        if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
            if layer.zindex != zindex {
                layer.zindex = zindex;
                self.sort_layers();
                self.force_redraw = true;
            }
        }
    }

    pub fn focus(&mut self, id: impl Into<String>) {
        self.focused = Some(id.into());
    }

    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    pub fn component(&self, id: &str) -> Option<&dyn Component> {
        self.layers
            .iter()
            .find(|l| l.id == id)
            .map(|l| l.component.as_ref())
    }

    pub fn component_mut(&mut self, id: &str) -> Option<&mut dyn Component> {
        for layer in &mut self.layers {
            if layer.id == id {
                return Some(&mut *layer.component);
            }
        }
        None
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.current.resize(width, height);
        self.previous.resize(width, height);
        for layer in &mut self.layers {
            layer.component.mark_dirty();
        }
        self.force_redraw = true;
    }

    pub fn render<W: Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.current.clear_all();

        let focused_id = self.focused.clone();
        for layer in &self.layers {
            if self.force_redraw || layer.component.is_dirty() {
                let ctx = DrawContext {
                    terminal_width: self.width,
                    terminal_height: self.height,
                    focused: focused_id.as_deref() == Some(&layer.id),
                };
                let mut slice = self.current.slice_mut(layer.rect);
                layer.component.draw(layer.rect, &mut slice, &ctx);
            } else {
                copy_region(&self.previous, &mut self.current, layer.rect);
            }
        }

        w.queue(BeginSynchronizedUpdate)?;

        if self.force_redraw {
            flush_full(&self.current, w)?;
        } else {
            flush_diff(w, self.current.diff(&self.previous))?;
        }

        let cursor_pos = focused_id.as_deref().and_then(|fid| {
            self.layers.iter().find(|l| l.id == fid).and_then(|l| {
                l.component
                    .cursor()
                    .map(|(cx, cy)| (l.rect.left + cx, l.rect.top + cy))
            })
        });
        if let Some((x, y)) = cursor_pos {
            w.queue(crossterm::cursor::Show)?;
            w.queue(crossterm::cursor::MoveTo(x, y))?;
        } else {
            w.queue(crossterm::cursor::Hide)?;
        }

        w.queue(EndSynchronizedUpdate)?;
        w.flush()?;

        self.current.swap_with(&mut self.previous);
        for layer in &mut self.layers {
            layer.component.mark_clean();
        }
        self.force_redraw = false;

        Ok(())
    }

    pub fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyResult {
        if let Some(ref focused_id) = self.focused {
            if let Some(layer) = self.layers.iter_mut().find(|l| &l.id == focused_id) {
                let result = layer.component.handle_key(code, mods);
                if result != KeyResult::Ignored {
                    return result;
                }
            }
        }
        KeyResult::Ignored
    }

    pub fn force_redraw(&mut self) {
        self.force_redraw = true;
    }

    pub fn layer_ids(&self) -> Vec<&str> {
        self.layers.iter().map(|l| l.id.as_str()).collect()
    }

    fn sort_layers(&mut self) {
        self.layers.sort_by_key(|l| l.zindex);
    }
}

fn copy_region(src: &Grid, dst: &mut Grid, area: Rect) {
    for y in area.top..area.bottom().min(src.height()).min(dst.height()) {
        for x in area.left..area.right().min(src.width()).min(dst.width()) {
            let cell = src.cell(x, y);
            *dst.cell_mut(x, y) = cell.clone();
        }
    }
}

fn flush_full<W: Write>(grid: &Grid, w: &mut W) -> std::io::Result<()> {
    use crate::grid::Style;
    use crossterm::cursor::MoveTo;
    use crossterm::style::{
        Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    };

    let mut current_style = Style::default();
    for y in 0..grid.height() {
        w.queue(MoveTo(0, y))?;
        for x in 0..grid.width() {
            let cell = grid.cell(x, y);
            if cell.style != current_style {
                w.queue(SetAttribute(Attribute::Reset))?;
                w.queue(ResetColor)?;
                if let Some(fg) = cell.style.fg {
                    w.queue(SetForegroundColor(fg))?;
                }
                if let Some(bg) = cell.style.bg {
                    w.queue(SetBackgroundColor(bg))?;
                }
                if cell.style.bold {
                    w.queue(SetAttribute(Attribute::Bold))?;
                }
                if cell.style.dim {
                    w.queue(SetAttribute(Attribute::Dim))?;
                }
                if cell.style.italic {
                    w.queue(SetAttribute(Attribute::Italic))?;
                }
                if cell.style.underline {
                    w.queue(SetAttribute(Attribute::Underlined))?;
                }
                if cell.style.crossedout {
                    w.queue(SetAttribute(Attribute::CrossedOut))?;
                }
                current_style = cell.style;
            }
            let mut buf = [0u8; 4];
            let s = cell.symbol.encode_utf8(&mut buf);
            w.queue(Print(s.to_string()))?;
        }
    }
    w.queue(SetAttribute(Attribute::Reset))?;
    w.queue(ResetColor)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{GridSlice, Style};

    struct TestComponent {
        text: String,
        style: Style,
        dirty: bool,
    }

    impl TestComponent {
        fn new(text: &str) -> Self {
            Self {
                text: text.to_string(),
                style: Style::default(),
                dirty: true,
            }
        }
    }

    impl Component for TestComponent {
        fn draw(&self, _area: Rect, grid: &mut GridSlice<'_>, _ctx: &DrawContext) {
            grid.put_str(0, 0, &self.text, self.style);
        }

        fn handle_key(&mut self, _code: KeyCode, _mods: KeyModifiers) -> KeyResult {
            KeyResult::Ignored
        }

        fn is_dirty(&self) -> bool {
            self.dirty
        }

        fn mark_dirty(&mut self) {
            self.dirty = true;
        }

        fn mark_clean(&mut self) {
            self.dirty = false;
        }
    }

    #[test]
    fn add_and_render_component() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "test",
            Box::new(TestComponent::new("hello")),
            Rect::new(0, 0, 20, 1),
            0,
        );
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("hello"));
    }

    #[test]
    fn remove_component() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(TestComponent::new("aaa")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        comp.add(
            "b",
            Box::new(TestComponent::new("bbb")),
            Rect::new(1, 0, 10, 1),
            0,
        );
        assert!(comp.remove("a").is_some());
        assert_eq!(comp.layer_ids(), vec!["b"]);
    }

    #[test]
    fn z_order_respected() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "back",
            Box::new(TestComponent::new("BACK")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        comp.add(
            "front",
            Box::new(TestComponent::new("FRONT")),
            Rect::new(0, 0, 10, 1),
            10,
        );
        assert_eq!(comp.layer_ids(), vec!["back", "front"]);
    }

    #[test]
    fn focus_routes_keys() {
        let mut comp = Compositor::new(20, 5);

        struct ConsumeAll {
            dirty: bool,
        }
        impl Component for ConsumeAll {
            fn draw(&self, _: Rect, _: &mut GridSlice<'_>, _: &DrawContext) {}
            fn handle_key(&mut self, _: KeyCode, _: KeyModifiers) -> KeyResult {
                KeyResult::Consumed
            }
            fn is_dirty(&self) -> bool {
                self.dirty
            }
            fn mark_dirty(&mut self) {
                self.dirty = true;
            }
            fn mark_clean(&mut self) {
                self.dirty = false;
            }
        }

        comp.add(
            "a",
            Box::new(ConsumeAll { dirty: true }),
            Rect::new(0, 0, 10, 1),
            0,
        );
        assert_eq!(
            comp.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyResult::Ignored
        );
        comp.focus("a");
        assert_eq!(
            comp.handle_key(KeyCode::Char('x'), KeyModifiers::NONE),
            KeyResult::Consumed
        );
    }

    #[test]
    fn resize_marks_all_dirty() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(TestComponent::new("hi")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert!(!comp.layers[0].component.is_dirty());
        comp.resize(40, 10);
        assert!(comp.layers[0].component.is_dirty());
    }

    #[test]
    fn set_rect_marks_dirty() {
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(TestComponent::new("hi")),
            Rect::new(0, 0, 10, 1),
            0,
        );
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert!(!comp.layers[0].component.is_dirty());
        comp.set_rect("a", Rect::new(1, 0, 10, 1));
        assert!(comp.layers[0].component.is_dirty());
    }

    #[test]
    fn clean_components_not_redrawn() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        struct CountingComponent {
            draw_count: Arc<AtomicU32>,
            dirty: bool,
        }
        impl Component for CountingComponent {
            fn draw(&self, _: Rect, _: &mut GridSlice<'_>, _: &DrawContext) {
                self.draw_count.fetch_add(1, Ordering::Relaxed);
            }
            fn handle_key(&mut self, _: KeyCode, _: KeyModifiers) -> KeyResult {
                KeyResult::Ignored
            }
            fn is_dirty(&self) -> bool {
                self.dirty
            }
            fn mark_dirty(&mut self) {
                self.dirty = true;
            }
            fn mark_clean(&mut self) {
                self.dirty = false;
            }
        }

        let count = Arc::new(AtomicU32::new(0));
        let mut comp = Compositor::new(20, 5);
        comp.add(
            "a",
            Box::new(CountingComponent {
                draw_count: count.clone(),
                dirty: true,
            }),
            Rect::new(0, 0, 10, 1),
            0,
        );

        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);

        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cursor_position_from_focused() {
        struct CursorComp {
            dirty: bool,
        }
        impl Component for CursorComp {
            fn draw(&self, _: Rect, _: &mut GridSlice<'_>, _: &DrawContext) {}
            fn handle_key(&mut self, _: KeyCode, _: KeyModifiers) -> KeyResult {
                KeyResult::Ignored
            }
            fn cursor(&self) -> Option<(u16, u16)> {
                Some((3, 1))
            }
            fn is_dirty(&self) -> bool {
                self.dirty
            }
            fn mark_dirty(&mut self) {
                self.dirty = true;
            }
            fn mark_clean(&mut self) {
                self.dirty = false;
            }
        }

        let mut comp = Compositor::new(20, 10);
        comp.add(
            "edit",
            Box::new(CursorComp { dirty: true }),
            Rect::new(5, 10, 10, 3),
            0,
        );
        comp.focus("edit");
        let mut out = Vec::new();
        comp.render(&mut out).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[?25h"));
    }
}
