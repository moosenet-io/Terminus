//! DPROMPT-01: Trait vector and trait-to-prompt mapping.
//!
//! Lumina's personality is parameterised by four floats in the range
//! `0.15..=0.85`.  Each float is mapped to a short, range-appropriate
//! behavioural instruction (~5 tokens) that becomes the `[style]` layer of
//! the assembled system prompt (see [`super::PromptAssembler`]).
//!
//! The vector is persisted as JSON at
//! `{layers_root}/{user_id}/trait-vector.json`.  It is updated daily by the
//! sleep-time trait tuner (DPROMPT-02) and read on every turn by the
//! assembler.  Values are always clamped to the soft bounds on load and save
//! so a corrupted or out-of-range file can never produce an invalid prompt.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Soft lower bound for every trait.
pub const TRAIT_MIN: f32 = 0.15;
/// Soft upper bound for every trait.
pub const TRAIT_MAX: f32 = 0.85;

/// Initial trait values for a brand-new user (the locked-in personality).
pub const INIT_FLAIR: f32 = 0.70;
pub const INIT_SPONTANEITY: f32 = 0.55;
pub const INIT_HUMOR: f32 = 0.65;
pub const INIT_FOCUS: f32 = 0.75;

/// The four self-tuning personality traits.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TraitVector {
    /// Stylistic expression, vivid language, personality in word choice.
    pub flair: f32,
    /// Unsolicited commentary, proactive suggestions, connecting dots.
    pub spontaneity: f32,
    /// Wit, wordplay, playful tone — natural, never forced.
    pub humor: f32,
    /// Task adherence, leading with the answer, concise delivery.
    pub focus: f32,
}

impl Default for TraitVector {
    fn default() -> Self {
        TraitVector {
            flair: INIT_FLAIR,
            spontaneity: INIT_SPONTANEITY,
            humor: INIT_HUMOR,
            focus: INIT_FOCUS,
        }
    }
}

/// Clamp a single trait value to the soft bounds.
#[inline]
pub fn clamp_trait(v: f32) -> f32 {
    if v.is_nan() {
        // A NaN can never satisfy the bounds; reset to the midpoint.
        return (TRAIT_MIN + TRAIT_MAX) / 2.0;
    }
    v.clamp(TRAIT_MIN, TRAIT_MAX)
}

impl TraitVector {
    /// Return a copy with every field clamped to `0.15..=0.85`.
    pub fn clamped(self) -> Self {
        TraitVector {
            flair: clamp_trait(self.flair),
            spontaneity: clamp_trait(self.spontaneity),
            humor: clamp_trait(self.humor),
            focus: clamp_trait(self.focus),
        }
    }

    /// Load a trait vector from `path`.
    ///
    /// On any failure (missing file, unreadable, corrupt JSON) returns the
    /// default vector — the assembler can therefore never fail because of a
    /// bad trait file.  Loaded values are always clamped.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<TraitVector>(&s) {
                Ok(tv) => tv.clamped(),
                Err(e) => {
                    log::warn!("trait-vector.json corrupt ({e}); resetting to defaults");
                    TraitVector::default()
                }
            },
            Err(_) => TraitVector::default(),
        }
    }

    /// Persist this vector (clamped) to `path`, creating parent dirs.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let clamped = self.clamped();
        let json = serde_json::to_string_pretty(&clamped)
            .unwrap_or_else(|_| "{}".to_string());
        std::fs::write(path, json)
    }

    /// Map this vector to a single-line `[style]` behavioural instruction.
    ///
    /// One short phrase per trait, joined with `". "`, e.g.
    /// `"Vivid, personality in every line. Connect dots, volunteer insights. \
    ///   Naturally witty, playful. Laser-focused, answer first always."`
    pub fn to_instructions(&self) -> String {
        let c = self.clamped();
        format!(
            "{}. {}. {}. {}.",
            flair_text(c.flair),
            spontaneity_text(c.spontaneity),
            humor_text(c.humor),
            focus_text(c.focus),
        )
    }
}

/// Which of the four bands a value falls into.  Bands are lower-inclusive:
/// `[0.15,0.30) [0.30,0.50) [0.50,0.70) [0.70,0.85]`.
fn band(v: f32) -> usize {
    let v = clamp_trait(v);
    if v < 0.30 {
        0
    } else if v < 0.50 {
        1
    } else if v < 0.70 {
        2
    } else {
        3
    }
}

fn flair_text(v: f32) -> &'static str {
    ["Plain and clinical",
     "Clear, minimal style",
     "Expressive, some color",
     "Vivid, personality in every line"][band(v)]
}

fn spontaneity_text(v: f32) -> &'static str {
    ["Only answer what's asked",
     "Occasionally mention related info",
     "Connect dots, volunteer insights",
     "Proactive, anticipate needs"][band(v)]
}

fn humor_text(v: f32) -> &'static str {
    ["Strictly professional",
     "Light, rare wit",
     "Naturally witty, playful",
     "Fun, charming, genuine humor"][band(v)]
}

fn focus_text(v: f32) -> &'static str {
    ["Explore freely, tangents welcome",
     "Stay mostly on topic",
     "Lead with the answer, brief asides ok",
     "Laser-focused, answer first always"][band(v)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_matches_locked_in_values() {
        let t = TraitVector::default();
        assert_eq!(t.flair, 0.70);
        assert_eq!(t.spontaneity, 0.55);
        assert_eq!(t.humor, 0.65);
        assert_eq!(t.focus, 0.75);
    }

    #[test]
    fn clamp_enforces_bounds() {
        let t = TraitVector { flair: 1.5, spontaneity: -3.0, humor: 0.85, focus: 0.15 }.clamped();
        assert_eq!(t.flair, TRAIT_MAX);
        assert_eq!(t.spontaneity, TRAIT_MIN);
        assert_eq!(t.humor, 0.85);
        assert_eq!(t.focus, 0.15);
    }

    #[test]
    fn nan_resets_to_midpoint() {
        assert_eq!(clamp_trait(f32::NAN), 0.5);
    }

    #[test]
    fn bands_cover_each_range() {
        assert_eq!(band(0.15), 0);
        assert_eq!(band(0.29), 0);
        assert_eq!(band(0.30), 1);
        assert_eq!(band(0.49), 1);
        assert_eq!(band(0.50), 2);
        assert_eq!(band(0.69), 2);
        assert_eq!(band(0.70), 3);
        assert_eq!(band(0.85), 3);
    }

    #[test]
    fn instructions_for_initial_values() {
        let t = TraitVector::default();
        let s = t.to_instructions();
        // flair 0.70 → top band, focus 0.75 → top band
        assert!(s.contains("Vivid, personality in every line"));
        assert!(s.contains("Connect dots, volunteer insights")); // spontaneity 0.55
        assert!(s.contains("Naturally witty, playful")); // humor 0.65
        assert!(s.contains("Laser-focused, answer first always")); // focus 0.75
    }

    #[test]
    fn instructions_for_low_values() {
        let t = TraitVector { flair: 0.15, spontaneity: 0.20, humor: 0.20, focus: 0.20 };
        let s = t.to_instructions();
        assert!(s.contains("Plain and clinical"));
        assert!(s.contains("Only answer what's asked"));
        assert!(s.contains("Strictly professional"));
        assert!(s.contains("Explore freely, tangents welcome"));
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir().unwrap();
        let t = TraitVector::load(&dir.path().join("nope.json"));
        assert_eq!(t, TraitVector::default());
    }

    #[test]
    fn load_corrupt_file_returns_default() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("trait-vector.json");
        std::fs::write(&p, "{not valid json").unwrap();
        assert_eq!(TraitVector::load(&p), TraitVector::default());
    }

    #[test]
    fn save_then_load_roundtrip_clamps() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sub").join("trait-vector.json");
        let t = TraitVector { flair: 2.0, spontaneity: 0.55, humor: 0.65, focus: 0.75 };
        t.save(&p).unwrap();
        let loaded = TraitVector::load(&p);
        assert_eq!(loaded.flair, TRAIT_MAX); // clamped on save
        assert_eq!(loaded.spontaneity, 0.55);
    }
}
