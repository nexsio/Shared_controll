use crate::KEY_LEN;

/// Verwaltung vertrauenswürdiger Partnerschlüssel. Eine dauerhafte Ablage (Schlüsselbund) folgt später.
pub trait TrustStore {
    fn is_trusted(&self, key: &[u8; KEY_LEN]) -> bool;
    fn trust(&mut self, key: [u8; KEY_LEN]);
    fn revoke(&mut self, key: &[u8; KEY_LEN]);
}

#[derive(Debug, Default)]
pub struct MemoryTrustStore {
    keys: Vec<[u8; KEY_LEN]>,
}

impl TrustStore for MemoryTrustStore {
    fn is_trusted(&self, key: &[u8; KEY_LEN]) -> bool {
        self.keys.contains(key)
    }

    fn trust(&mut self, key: [u8; KEY_LEN]) {
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }

    fn revoke(&mut self, key: &[u8; KEY_LEN]) {
        self.keys.retain(|k| k != key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_and_revoke() {
        let mut store = MemoryTrustStore::default();
        let key = [7u8; KEY_LEN];
        assert!(!store.is_trusted(&key));
        store.trust(key);
        store.trust(key);
        assert!(store.is_trusted(&key));
        store.revoke(&key);
        assert!(!store.is_trusted(&key));
    }
}
