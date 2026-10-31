#![doc = include_str!("../README.md")]

pub mod ansi;
pub mod color_depth;
pub mod compositor;
pub mod flush;
pub mod geometry;
pub mod grid;
pub mod hit;
pub mod kitty;
pub mod layout;
pub mod line;
pub mod session;
pub mod snapshot;
pub mod surface;

pub use color_depth::{color_depth, detect_color_depth, ColorDepth, COLOR_ENV};
pub use compositor::Compositor;
pub use flush::{flush_diff, flush_diff_at};
pub use geometry::Insets;
pub use grid::{
    display_width, truncate_width, Cell, CellSymbol, CellUpdate, Grid, GridSlice, Style, TextAlign,
};
pub use hit::HitRegistry;
pub use layout::{
    Axis, Border, Constraint, ContainerId, DividerStyles, LayoutPaintOp, LayoutStyle, LayoutTree,
    LeafSizer, Natural, NaturalRef, NoopSizer, PaintDispatch, PaintId, Rect, ResolvedLayout,
    ResolvedSplit, Split, SplitId, SplitInteraction, SplitOptions, SplitPane, SplitPhase,
    SplitRatio, SplitResizeMode, SplitResponse, SplitSize, StaticNatural,
};
pub use line::{Line, Span};
pub use session::{SuspendScreen, TerminalSession, TerminalSessionBuilder};
pub use smelt_style::image::RasterImage;
pub use smelt_style::style::Color;
pub use smelt_style::theme::Theme;
pub use snapshot::SnapshotFrame;
pub use surface::Surface;
