//! Pure-term storybook - visual snapshot tests over the public renderer
//! API. No editor surface, no buffers, no overlay machinery; each story
//! drives `Compositor` + `ResolvedLayout::paint` directly.

#[macro_use]
mod storybook;
