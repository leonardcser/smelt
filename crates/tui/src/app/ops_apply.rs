//! Drains the deferred-closure queue. Each closure was pushed by a
//! Rust dialog callback that held `&mut Ui` at fire time and needs
//! `&mut App` access after dispatch returns.

use super::*;

impl App {
    pub(super) fn apply_ops(&mut self, ops: Vec<crate::app::ops::Deferred>) {
        for f in ops {
            f(self);
        }
    }
}
