//! Turning an operator's password into something the roster can hold, and
//! checking one against it.
//!
//! `[remote_access]` keeps its password in the clear and always has. That is
//! defensible for a single-operator station: the secret belongs to whoever can
//! read the config directory anyway, and the manual says so. A roster is a
//! different thing — those are *other people's* passwords, quite possibly the
//! one they use elsewhere, and a station that holds them in the clear is a
//! station that leaks them if anything on the machine ever goes wrong.
//!
//! So `users.toml` holds Argon2id PHC strings. The cost parameters are the
//! `argon2` crate's defaults (19 MiB, three passes), which is a tenth of a
//! second on a Raspberry Pi — irrelevant next to the sign-in gate's
//! three-second lockout, and expensive enough that a stolen file is not a word
//! list away from being a password.

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand_core::OsRng;
use tracing::warn;

/// A hash of a password that is not any password, for the case where there is
/// nothing to check against.
///
/// [`verify`] is given this when the username was not on the roster, so an
/// unknown name costs the same Argon2 verification as a known one. Without it
/// the two are a tenth of a second apart, which is all anybody needs to sort a
/// list of callsigns into those the station knows and those it does not.
///
/// Generated once, from a password nobody has: the salt and the digest are
/// fixed text here because what matters is only that it parses and never
/// matches.
const NOBODY: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2Ryb3hpZGVub2JvZHlzYWx0$\
                      3S7Wm9OZ6yPBqQZ1Sxlz7Hs2vGxT4tEwYqPvVJUdKgo";

/// Hash a password for the roster.
///
/// Fails only if the platform has no randomness to make a salt out of, which is
/// not a condition to paper over: a fixed salt would make two operators who
/// picked the same password visibly the same in the file.
pub fn hash(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("cannot hash the password: {e}"))
}

/// Whether `password` is the one `stored` was made from.
///
/// An empty or unparseable `stored` is checked against [`NOBODY`] rather than
/// refused outright, so the time this takes does not report which it was. It
/// still answers no.
pub fn verify(stored: &str, password: &str) -> bool {
    let parsed = PasswordHash::new(stored);
    if parsed.is_err() && !stored.is_empty() {
        // Worth a word in the log: an entry whose hash cannot be read is one
        // whose operator is being turned away for a reason that is not their
        // password, and they have no way to find that out from the client.
        warn!("a roster entry's password hash cannot be read; that operator cannot sign in");
    }
    // Built either way rather than only in the error arm, so that the work done
    // before the verification does not depend on which arm this is.
    let fallback = PasswordHash::new(NOBODY).expect("the fixed non-hash parses");
    let hash = parsed.as_ref().unwrap_or(&fallback);
    Argon2::default().verify_password(password.as_bytes(), hash).is_ok()
}

/// The work [`verify`] does when there is nobody to verify against.
///
/// Called for a username that is not on the roster, purely so that case costs
/// the same as one that is. The answer is discarded — and is always false.
pub fn verify_nobody(password: &str) {
    let _ = verify(NOBODY, password);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hashed_password_verifies_and_a_wrong_one_does_not() {
        let stored = hash("hunter2").expect("hash");
        assert!(stored.starts_with("$argon2id$"));
        assert!(verify(&stored, "hunter2"));
        assert!(!verify(&stored, "hunter3"));
        assert!(!verify(&stored, ""));
    }

    /// Two operators who chose the same password do not look alike in the file.
    #[test]
    fn the_same_password_hashes_differently_every_time() {
        assert_ne!(hash("hunter2").expect("hash"), hash("hunter2").expect("hash"));
    }

    /// The three shapes of "there is nothing to check against" all answer no,
    /// and none of them answers it by panicking.
    #[test]
    fn nothing_to_check_against_is_a_refusal() {
        assert!(!verify("", "hunter2"));
        assert!(!verify("not a PHC string", "hunter2"));
        assert!(!verify(NOBODY, "hunter2"));
        assert!(!verify(NOBODY, ""));
        verify_nobody("hunter2");
    }
}
