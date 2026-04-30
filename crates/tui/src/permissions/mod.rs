//! Permission policy capability.
//!
//! Today this module hosts the workspace JSON store
//! ([`store`]) — load / save / add_tool / add_dir / into_approvals over
//! `~/.local/state/smelt/workspaces/<encoded-cwd>/permissions.json`.
//!
//! P5.c absorbs the rest of `engine/permissions/` here: bash AST
//! parsing, ruleset matching, workspace boundary check, and the
//! runtime approvals table. At that point engine becomes policy-free
//! and Lua tool hooks compose the pieces directly.

pub mod store;
