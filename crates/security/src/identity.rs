use std::fmt;

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::SecurityError;

pub const KEY_LEN: usize = 32;

const IDENTITY_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Langzeitidentität eines Geräts (X25519). Der private Schlüssel wird beim Löschen überschrieben.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct Identity {
    private: [u8; KEY_LEN],
    #[zeroize(skip)]
    public: [u8; KEY_LEN],
}

impl Identity {
    pub fn generate() -> Result<Self, SecurityError> {
        let params = IDENTITY_PARAMS.parse()?;
        let keypair = snow::Builder::new(params).generate_keypair()?;
        Ok(Self {
            private: keypair.private.as_slice().try_into().map_err(|_| SecurityError::Noise)?,
            public: keypair.public.as_slice().try_into().map_err(|_| SecurityError::Noise)?,
        })
    }

    /// Stellt eine zuvor gespeicherte Identität wieder her; `public` muss zu `private` gehören.
    pub fn from_keys(private: [u8; KEY_LEN], public: [u8; KEY_LEN]) -> Self {
        Self { private, public }
    }

    /// Nur zum Speichern in einer gegen fremden Zugriff geschützten Datei verwenden.
    pub fn export_private(&self) -> [u8; KEY_LEN] {
        self.private
    }

    pub fn public(&self) -> &[u8; KEY_LEN] {
        &self.public
    }

    pub(crate) fn private(&self) -> &[u8; KEY_LEN] {
        &self.private
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("public", &self.public).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_identities_differ() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.public(), b.public());
        assert_ne!(a.private(), b.private());
    }

    #[test]
    fn debug_output_hides_private_key() {
        let id = Identity::generate().unwrap();
        let text = format!("{id:?}");
        assert!(!text.contains("private"));
        for byte in id.private().windows(4) {
            let needle = format!("{byte:?}");
            assert!(!text.contains(&needle));
        }
    }
}
