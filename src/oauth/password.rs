//! Password hashing and verification for the OAuth login step (RMCP-03).
//!
//! ## Why argon2id, and why the parameters are not ours
//! Account passwords are human-chosen and therefore low-entropy: the attack to
//! defend against is an offline dictionary run over a stolen `rmcp_account`
//! table, not a cryptanalysis of the hash. That calls for a memory-hard KDF,
//! and argon2id is the current consensus answer (it resists both the GPU
//! parallelism argon2i is weak to and the side channels argon2d is weak to).
//!
//! The cost parameters are deliberately [`argon2::Argon2::default`]'s — m=19456
//! KiB, t=2, p=1 — rather than values chosen here. Those are the argon2 crate's
//! tracking of the OWASP recommendation, so they move when the recommendation
//! moves, whereas a number written into this file would be frozen at whatever
//! was reasonable on the day it was typed. Every hash carries its own
//! parameters in its PHC string, so a future default change re-verifies old
//! hashes correctly and only new hashes get the new cost.
//!
//! ## The indistinguishability requirement
//! An unknown account and a wrong password must be indistinguishable to the
//! caller — in the response body, in the status code, and in TIMING. The first
//! two are the handler's job. The third is this module's, and it is the one
//! that is easy to get wrong: the obvious implementation returns early when the
//! account lookup misses, skipping the ~40ms argon2 verification entirely, and
//! the resulting timing gap is a reliable account-existence oracle that a
//! remote attacker can measure over a handful of requests.
//!
//! [`dummy_hash`] exists for exactly that: when there is no account, the caller
//! verifies the submitted password against a real argon2id hash with the same
//! parameters, discards the result, and answers identically. See
//! [`verify_password`]'s docs for the shape the caller must use.
//!
//! ## What is NOT here: TOTP
//! The item asks for TOTP verification alongside the password. It is not
//! implemented, and the reason is not scope: `rmcp_account.totp_secret_enc`
//! holds a seed *encrypted* with a subkey derived from the OAuth signing key,
//! and nothing in the tree derives or applies that subkey yet — RMCP-08 owns
//! provisioning it. A verifier cannot check a code against a seed it cannot
//! decrypt.
//!
//! The choice is therefore between failing OPEN (log a 2FA account in on its
//! password alone, silently downgrading it to one factor) and failing CLOSED
//! (refuse the login until the second factor can actually be checked). This
//! module fails closed: [`requires_unavailable_second_factor`] reports the
//! condition and the handler refuses. That denies service to a 2FA account
//! rather than weakening it, which is the correct direction for the failure —
//! and it is loud, so it cannot be mistaken for working 2FA.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use std::sync::OnceLock;
use uuid::Uuid;

use crate::error::ToolError;
use crate::oauth::Argon2idHash;

/// Hash a plaintext password into a storable [`Argon2idHash`].
///
/// The salt is 16 bytes taken from a v4 UUID. A salt needs uniqueness, not
/// secrecy — its whole job is to stop one precomputed table from attacking
/// every row — and a v4 UUID is 122 bits of CSPRNG output, which makes a
/// collision across any credible number of accounts impossible in practice.
/// Sourcing it this way rather than through a second RNG dependency keeps the
/// crate's randomness surface to the one generator it already uses.
pub fn hash_password(plaintext: &str) -> Result<Argon2idHash, ToolError> {
    let salt_bytes = *Uuid::new_v4().as_bytes();
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|_| ToolError::Execution("could not encode a password salt".into()))?;
    let phc = Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        // The error is not interpolated: argon2's error text is generic, but a
        // hashing failure is the one place a careless `{e}` could end up next
        // to the value being hashed in a future refactor.
        .map_err(|_| ToolError::Execution("password hashing failed".into()))?
        .to_string();
    Argon2idHash::parse(&phc)
}

/// Verify a plaintext password against a stored PHC string.
///
/// Returns `false` for a wrong password AND for a stored value that is not a
/// parseable hash — a corrupt or truncated row must deny, never admit.
///
/// ## The call shape this function requires
/// The comparison inside `verify_password` is constant-time with respect to the
/// digest, but that is not sufficient on its own. The caller must ALSO do the
/// same work when there is no account at all:
///
/// ```ignore
/// let stored = account.as_ref().map(|a| a.password_hash.as_str());
/// let matched = verify_password(submitted, stored.unwrap_or_else(dummy_hash));
/// if !(matched && account.is_some()) { /* one generic failure path */ }
/// ```
///
/// Note that `account.is_some()` is evaluated AFTER the verification, not
/// short-circuited before it. Written the other way round — `account.is_some()
/// && verify_password(..)` — Rust's `&&` skips the hash entirely for an unknown
/// account, and the timing oracle is back.
pub fn verify_password(plaintext: &str, stored_phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_phc) else {
        return false;
    };
    Argon2::default().verify_password(plaintext.as_bytes(), &parsed).is_ok()
}

/// A real argon2id hash, of a value no account uses, for the caller to verify
/// against when there is no account.
///
/// Computed once on first use and cached. It must be a GENUINE hash produced by
/// the same hasher — a hand-written constant PHC string would work for the
/// parse, but if its cost parameters ever drifted from the live defaults the
/// dummy path would become measurably faster or slower than the real one and
/// the oracle would reopen. Deriving it from the same [`hash_password`] the
/// real accounts use makes that drift impossible.
///
/// The pre-image is a fresh random UUID rather than a fixed string: nobody can
/// deliberately submit the password that "matches" the dummy hash, so a
/// spurious `true` from the dummy path cannot be induced. (It would be harmless
/// anyway — the caller ANDs the result with "an account was found" — but a
/// property that holds for two independent reasons is worth having.)
///
/// If hashing itself fails, this falls back to a structurally valid PHC string
/// that no password can match. That degrades the timing defence rather than
/// failing the login path, which is the right trade: `verify_password` against
/// it still returns `false`, so the fallback can only ever deny.
pub fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| {
        let preimage = Uuid::new_v4().to_string();
        match hash_password(&preimage) {
            Ok(hash) => hash.as_str().to_string(),
            Err(_) => {
                "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$\
                 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    .to_string()
            }
        }
    })
}

/// Whether this account carries a second factor that this door cannot yet
/// verify (see the module docs).
///
/// Takes the stored ciphertext rather than the whole account so the check reads
/// the same whether the caller has a row or just the column, and so it can be
/// tested without constructing a database row.
pub fn requires_unavailable_second_factor(totp_secret_enc: Option<&[u8]>) -> bool {
    // An EMPTY ciphertext is treated as "no second factor", matching the
    // crate-wide rule that a blank materialized value is an absent one. A
    // zero-length ciphertext cannot decrypt to a seed, so treating it as
    // present would lock out an account for a column that holds nothing.
    matches!(totp_secret_enc, Some(bytes) if !bytes.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The round trip has to work, or nothing else in this module matters.
    #[test]
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let plaintext = "a-long-enough-passphrase-for-a-test";
        let hashed = hash_password(plaintext).expect("hashing must succeed");
        assert!(verify_password(plaintext, hashed.as_str()));
        assert!(!verify_password("not-the-passphrase", hashed.as_str()));
    }

    /// The stored value must never be the plaintext, and must be structurally
    /// argon2id — which is what [`Argon2idHash::parse`] enforces, so this also
    /// proves the real hasher's output survives RMCP-01's guard.
    #[test]
    fn the_stored_value_is_a_real_argon2id_phc_string() {
        let plaintext = "another-long-enough-passphrase";
        let hashed = hash_password(plaintext).expect("hashing must succeed");
        assert_ne!(hashed.as_str(), plaintext);
        assert!(hashed.as_str().starts_with("$argon2id$"));
        // Re-parsing the emitted string must succeed: if the hasher's output
        // ever stopped satisfying RMCP-01's structural check, every password
        // write would start failing in production.
        assert!(Argon2idHash::parse(hashed.as_str()).is_ok());
    }

    /// Two hashes of the same password must differ — otherwise the salt is not
    /// doing its job and one precomputed table attacks every account.
    #[test]
    fn the_salt_makes_two_hashes_of_one_password_differ() {
        let plaintext = "the-same-passphrase-twice";
        let first = hash_password(plaintext).expect("hashing must succeed");
        let second = hash_password(plaintext).expect("hashing must succeed");
        assert_ne!(first.as_str(), second.as_str());
        // …and both must still verify.
        assert!(verify_password(plaintext, first.as_str()));
        assert!(verify_password(plaintext, second.as_str()));
    }

    /// A corrupt, truncated or plaintext value in the column must DENY. The
    /// tempting implementation — fall back to a string comparison when the PHC
    /// parse fails — would turn a corrupted row into a plaintext password
    /// check.
    #[test]
    fn an_unparseable_stored_value_denies_rather_than_admitting() {
        for stored in ["", "hunter2", "$argon2id$", "not-a-hash-at-all"] {
            assert!(!verify_password("hunter2", stored), "must deny for {stored:?}");
            assert!(!verify_password("", stored), "must deny for {stored:?}");
        }
    }

    /// The dummy must be a genuine hash the real verifier accepts as input, or
    /// the no-account path would fail its parse early and skip the argon2 work
    /// — reopening the timing oracle it exists to close.
    #[test]
    fn the_dummy_hash_is_a_real_parseable_argon2id_hash() {
        let dummy = dummy_hash();
        assert!(Argon2idHash::parse(dummy).is_ok(), "dummy must be structurally valid");
        assert!(PasswordHash::new(dummy).is_ok(), "argon2 must be able to parse the dummy");
        // Stable across calls: it is cached, and a fresh hash per call would be
        // both slower and pointless.
        assert_eq!(dummy, dummy_hash());
    }

    /// Nothing a caller can submit may match the dummy.
    #[test]
    fn no_submitted_password_matches_the_dummy_hash() {
        for attempt in ["", "hunter2", "admin", dummy_hash()] {
            assert!(!verify_password(attempt, dummy_hash()));
        }
    }

    /// The dummy path must cost the SAME order of work as a real verification.
    /// Timing assertions are inherently noisy, so this deliberately asserts
    /// only the coarse property that actually matters — that the no-account
    /// path is not orders of magnitude faster, which is what an early return
    /// would make it. A tight bound here would be a flaky test, and a flaky
    /// security test gets deleted.
    #[test]
    fn the_dummy_path_does_comparable_work_to_a_real_verification() {
        use std::time::Instant;

        let real = hash_password("a-real-account-passphrase").expect("hashing must succeed");
        // Warm the lazy dummy and the hasher before measuring either arm.
        let _ = verify_password("warmup", dummy_hash());
        let _ = verify_password("warmup", real.as_str());

        let start = Instant::now();
        for _ in 0..3 {
            let _ = verify_password("wrong-passphrase", real.as_str());
        }
        let known_account = start.elapsed();

        let start = Instant::now();
        for _ in 0..3 {
            let _ = verify_password("wrong-passphrase", dummy_hash());
        }
        let unknown_account = start.elapsed();

        // Within a factor of ten in either direction. An early return would
        // make the unknown-account arm hundreds of times faster; ordinary
        // scheduler noise on a loaded build host will not.
        assert!(
            unknown_account * 10 >= known_account && known_account * 10 >= unknown_account,
            "the two arms must be the same timing class: known {known_account:?}, \
             unknown {unknown_account:?}"
        );
    }

    /// An account with a second factor must be REFUSED, not admitted on one
    /// factor, for as long as the seed cannot be decrypted.
    #[test]
    fn an_account_with_a_totp_seed_is_flagged_as_unverifiable() {
        assert!(requires_unavailable_second_factor(Some(&[1, 2, 3])));
        assert!(!requires_unavailable_second_factor(None));
        // A zero-length ciphertext holds no seed, so it is not a second factor
        // — treating it as one would lock an account out over an empty column.
        assert!(!requires_unavailable_second_factor(Some(&[])));
    }
}
