//! Thin, typed, mock-friendly HTTP clients for each service the media
//! domain orchestrates. MEDIA-01 scope only — see each submodule's doc
//! comment for its configuration and the one or two representative
//! operations it exposes. None of these register MCP tools themselves;
//! [`crate::media::register`] is the domain's only registration entry point,
//! and later items (MEDIA-02..07) are what actually build user-facing tools
//! on top of these clients.

pub mod <media-service>; // pii-test-fixture
pub mod plex;
pub mod prowlarr;
pub mod qtor;
pub mod radarr;
pub mod sonarr;
pub mod tmdb;
