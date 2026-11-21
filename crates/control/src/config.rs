//! Dauerhafte Ablage von Identität, vertrauten Partnern und Portal-Token unter `~/.config/shared-control`.

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use security::{Identity, TrustStore, KEY_LEN};

pub fn config_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "kein Konfigurationsverzeichnis"))?;
    let dir = base.join("shared-control");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(data)
}

/// Kurzkennung eines Partnerschlüssels für die Anzeige.
pub fn partner_label(key: &[u8; KEY_LEN]) -> String {
    key.iter().take(3).map(|byte| format!("{byte:02x}")).collect()
}

pub fn load_identity() -> io::Result<Identity> {
    let path = config_dir()?.join("identity.key");
    if let Ok(bytes) = fs::read(&path) {
        let corrupt = || io::Error::new(io::ErrorKind::InvalidData, "identity.key beschädigt");
        let (private, public) = bytes.split_at_checked(KEY_LEN).ok_or_else(corrupt)?;
        return Ok(Identity::from_keys(
            private.try_into().map_err(|_| corrupt())?,
            public.try_into().map_err(|_| corrupt())?,
        ));
    }
    let identity = Identity::generate().map_err(|e| io::Error::other(e.to_string()))?;
    let mut bytes = identity.export_private().to_vec();
    bytes.extend_from_slice(identity.public());
    write_private(&path, &bytes)?;
    Ok(identity)
}

/// Vertraute Partnerschlüssel, aneinandergereiht à 32 Bytes.
pub struct FileTrustStore {
    path: PathBuf,
    keys: Vec<[u8; KEY_LEN]>,
}

impl FileTrustStore {
    pub fn load() -> io::Result<Self> {
        let path = config_dir()?.join("trusted.keys");
        let keys = match fs::read(&path) {
            Ok(bytes) => bytes.chunks_exact(KEY_LEN).filter_map(|c| c.try_into().ok()).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self { path, keys })
    }

    /// Zuletzt gekoppelter Partner.
    pub fn latest(&self) -> Option<[u8; KEY_LEN]> {
        self.keys.last().copied()
    }

    fn save(&self) {
        let bytes: Vec<u8> = self.keys.iter().flatten().copied().collect();
        if let Err(e) = write_private(&self.path, &bytes) {
            eprintln!("Vertrauensliste konnte nicht gespeichert werden: {e}");
        }
    }
}

impl TrustStore for FileTrustStore {
    fn is_trusted(&self, key: &[u8; KEY_LEN]) -> bool {
        self.keys.contains(key)
    }

    fn trust(&mut self, key: [u8; KEY_LEN]) {
        self.keys.retain(|k| k != &key);
        self.keys.push(key);
        self.save();
    }

    fn revoke(&mut self, key: &[u8; KEY_LEN]) {
        self.keys.retain(|k| k != key);
        self.save();
    }
}
