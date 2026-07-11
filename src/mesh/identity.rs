//! Tailnet caller identity — plain data type (MESH-05).
//!
//! ## Why this type lives in its own, UNGATED module
//! [`TailnetIdentity`] is deliberately defined OUTSIDE the `tsnet` Cargo
//! feature gate that covers the rest of tailnet support
//! (`crate::mesh::tailnet`, whose `mod` declaration in `crate::mesh` is
//! `#[cfg(feature = "tsnet")]`). It holds only owned `String`/`Vec<String>`
//! fields — no `tsnet`/`libtailscale` types, no FFI handles — so there is no
//! technical reason it needs the feature gate, and a real reason it must NOT
//! have one: MESH-06 (the unified identity layer that reads this alongside
//! `crate::pki::mtls::ClientIdentity`) needs to compile and have its own
//! tests run on a plain default `cargo build`/`cargo test`, the same way
//! every other agent working on this crate does (this dev/build host has no
//! Go toolchain, so `--features tsnet` cannot even be compiled here — see
//! `crate::mesh::tailnet`'s module doc). Gating the TYPE would force MESH-06
//! itself behind `tsnet`, which is a much bigger scope leak than this one
//! module needing to exist unconditionally.
//!
//! The RESOLUTION logic (actually calling into `libtailscale`'s WhoIs) stays
//! fully gated in `crate::mesh::tailnet` — only this inert data shape is
//! shared outside the gate. See that module's doc for why WhoIs resolution
//! itself is still only partially wired as of this item.

/// A tailnet peer's resolved identity, as reported by `libtailscale`'s WhoIs
/// (once wired — see `crate::mesh::tailnet`'s module doc). Deliberately
/// minimal and non-secret: only what an authz decision (MESH-06) needs,
/// never a token, key, or anything else that must be redacted from logs.
///
/// `login`/`node` mirror `libtailscale`'s `WhoIsResponse.UserProfile.LoginName`
/// / `WhoIsResponse.Node.Name`; `tags` mirrors `WhoIsResponse.Node.Tags` (ACL
/// tags on the connecting node, e.g. `tag:ci`) — empty when the node carries
/// none.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TailnetIdentity {
    /// The tailnet login identity (e.g. an operator's tailnet account) that
    /// owns the connecting node.
    pub login: String,
    /// The connecting node's own tailnet machine name (MagicDNS name).
    pub node: String,
    /// ACL tags carried by the connecting node, if any (e.g. `tag:ci`).
    pub tags: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs on DEFAULT features (no `tsnet` needed) — confirms the type
    /// really is usable without the compile feature, per this module's doc.
    #[test]
    fn constructs_and_compares_on_default_features() {
        let a = TailnetIdentity {
            login: "<email>".to_string(), // pii-test-fixture
            node: "laptop.tailnetname.ts.net".to_string(), // pii-test-fixture
            tags: vec!["tag:ci".to_string()],
        };
        let b = a.clone();
        assert_eq!(a, b);
    }

    /// Holds no secret material: a `Debug` dump of a populated
    /// [`TailnetIdentity`] never contains anything that looks like a
    /// token/key/authkey — only the three plain structural fields.
    #[test]
    fn debug_output_has_no_secret_shaped_content() {
        let id = TailnetIdentity {
            login: "<email>".to_string(), // pii-test-fixture
            node: "laptop.tailnetname.ts.net".to_string(), // pii-test-fixture
            tags: vec!["tag:ci".to_string()],
        };
        let out = format!("{id:?}");
        for needle in ["authkey", "token", "secret", "password", "bearer"] {
            assert!(
                !out.to_ascii_lowercase().contains(needle),
                "Debug output unexpectedly contained {needle:?}: {out}"
            );
        }
    }

    #[test]
    fn default_is_empty() {
        let id = TailnetIdentity::default();
        assert_eq!(id.login, "");
        assert_eq!(id.node, "");
        assert!(id.tags.is_empty());
    }
}
