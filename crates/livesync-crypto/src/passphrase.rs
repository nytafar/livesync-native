//! The E2EE passphrase as a value: kept out of `Debug` output and wiped when
//! dropped.

use std::fmt;

use zeroize::Zeroizing;

/// An end-to-end encryption passphrase. Shows as `Passphrase(***)`.
#[derive(Clone, PartialEq, Eq)]
pub struct Passphrase(Zeroizing<String>);

impl Passphrase {
    pub fn new(s: impl Into<String>) -> Passphrase {
        Passphrase(Zeroizing::new(s.into()))
    }

    /// The passphrase itself, for key derivation and chunk IDs.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Passphrase(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_value() {
        let p = Passphrase::new("hunter2-throwaway");
        assert_eq!(format!("{p:?}"), "Passphrase(***)");
        assert_eq!(p.expose(), "hunter2-throwaway");
    }
}
