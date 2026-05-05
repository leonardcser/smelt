//! Window handle. `BufId` lives in `smelt_core::buffer` alongside the
//! Buffer it identifies; `WinId` stays here because windows are a
//! tui-only concept.

pub use smelt_core::buffer::{BufId, LUA_BUF_ID_BASE};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WinId(pub u64);

impl WinId {
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for WinId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "win:{}", self.0)
    }
}
