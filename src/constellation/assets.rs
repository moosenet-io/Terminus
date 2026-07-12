//! CONST-15: the `constellation-web` production bundle, EMBEDDED into the
//! `terminus_primary` binary at cargo-build time.
//!
//! ## Why embed instead of shipping the dist dir alongside the binary
//! Spec S115's `constellation-updater` build-on-dest pipeline rebuilds every
//! module (`terminus-primary` included) with a **cargo-only** `BUILD_CMD` on
//! the deploy host -- there is deliberately no npm/node toolchain in that
//! step (see the moosenet-spec v3.23 "constellation-updater (CI/CD)" note).
//! For the UI to survive that build with zero manual placement, the built
//! assets must already be inside the crate's source tree at the point
//! `cargo build` runs, and `rust-embed` folds them into the binary itself so
//! there is nothing to copy into place post-build either.
//!
//! ## Where the dist comes from
//! `constellation-web/dist/` is a **committed build artifact** (see
//! `constellation-web/README.md`'s "Embedded build (CONST-15)" section) --
//! NOT generated during the Rust build. A developer changing the UI must
//! rebuild it (`VITE_AGG_MODE=http npm run build`) and commit the result;
//! `cargo build` only ever reads what's already checked in.
//!
//! ## `#[folder]` path resolution
//! `rust-embed`'s `#[folder = "..."]` is resolved relative to
//! `CARGO_MANIFEST_DIR` at compile time (documented rust-embed behavior --
//! it expands to a `concat!(env!("CARGO_MANIFEST_DIR"), "/", $folder)` under
//! the hood), NOT relative to this source file or the process's runtime
//! working directory. This crate's `Cargo.toml` `[package]` lives at the
//! repo root (`terminus-rs`, a single-package "workspace" whose `[workspace]
//! members` are the two small `terminus-client`/`terminus-worker-sdk`
//! crates -- see that file's own doc comment), so `CARGO_MANIFEST_DIR` for
//! *this* crate IS the repo root, making `"constellation-web/dist"` resolve
//! to exactly the repo-root-relative path the built assets are committed
//! under -- verified by locating `constellation-web/` as a direct sibling of
//! this crate's own `Cargo.toml`, not nested under `src/`.
use rust_embed::RustEmbed;

/// The embedded `constellation-web/dist` production build. `RustEmbed`
/// derives `get(path) -> Option<EmbeddedFile>` and `iter() ->
/// impl Iterator<Item = Cow<'static, str>>` over ALL files under the folder
/// (asset hashes, `index.html`, everything Vite emitted) -- see
/// `crate::constellation::embedded_asset_response` for how they're served.
#[derive(RustEmbed)]
#[folder = "constellation-web/dist"]
pub struct WebAssets;

#[cfg(test)]
mod tests {
    use super::*;

    /// Load-bearing compile-time property: if the committed dist is ever
    /// deleted or the `#[folder]` path ever stops resolving, this is the
    /// cheapest test to catch it -- `index.html` must always be embedded.
    #[test]
    fn index_html_is_embedded() {
        assert!(WebAssets::get("index.html").is_some(), "constellation-web/dist/index.html must be embedded — did the dist get rebuilt/committed?");
    }
}
