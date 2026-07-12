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
//! `cargo build` runs, and `include_dir!` folds them into the binary itself
//! so there is nothing to copy into place post-build either.
//!
//! ## Why `include_dir` and not `rust-embed`
//! `rust-embed`'s derive macro (`rust-embed-impl`, a proc-macro) pulls
//! `mime_guess` into the HOST/proc-macro build context. On the build-on-dest
//! host that forces a fresh `mime_guess`/`zmij` build-script compile which
//! fails ("self-contained linker requested but not found in sysroot") --
//! `mime_guess` otherwise only ever appears as a cached `tower-http` target
//! dep. `include_dir` embeds the same tree with no `mime_guess` in its
//! dependency graph, so nothing new has to link. Content-Type is resolved by
//! a small in-crate extension table (`crate::constellation::content_type_for`)
//! rather than a MIME database -- a static Vite bundle only serves a handful
//! of extensions.
//!
//! ## Path resolution
//! `include_dir!("$CARGO_MANIFEST_DIR/constellation-web/dist")` is resolved at
//! compile time relative to this crate's `CARGO_MANIFEST_DIR`. This crate's
//! `Cargo.toml` `[package]` (`terminus-rs`) lives at the repo root, so
//! `CARGO_MANIFEST_DIR` IS the repo root and the path resolves to exactly the
//! repo-root-relative `constellation-web/dist` where the built assets are
//! committed -- `constellation-web/` is a direct sibling of this crate's
//! `Cargo.toml`, not nested under `src/`.
use include_dir::{include_dir, Dir};

/// The embedded `constellation-web/dist` production build: `index.html` plus
/// the hashed `assets/*` files Vite emitted. Look files up by their
/// root-relative path with [`Dir::get_file`] (works recursively into
/// `assets/`) -- see `crate::constellation::embedded_asset_response` for how
/// they're served.
pub static WEB_ASSETS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/constellation-web/dist");

#[cfg(test)]
mod tests {
    use super::*;

    /// Load-bearing compile-time property: if the committed dist is ever
    /// deleted or the `include_dir!` path ever stops resolving, this is the
    /// cheapest test to catch it -- `index.html` must always be embedded.
    #[test]
    fn index_html_is_embedded() {
        assert!(
            WEB_ASSETS.get_file("index.html").is_some(),
            "constellation-web/dist/index.html must be embedded — did the dist get rebuilt/committed?"
        );
    }
}
