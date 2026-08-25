# Compatibility debt

Compatibility code we intend to remove while smelt is alpha is marked with
`COMPAT(<id>)` and documented here.

## `lua-session-turn-block-idx`

`smelt.session.turns()` exposes deprecated `block_idx` as an alias of the
canonical `history_idx`. This prevents older rewind dialogs from passing a
missing value to `smelt.session.rewind_to()` and accidentally rewinding to the
start of the session. Remove the alias after third-party dialogs have migrated
to `history_idx` and the old field has passed through a documented deprecation
window.
