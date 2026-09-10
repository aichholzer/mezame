//! The master key on disk and the cipher over credential payloads.
//!
//! `~/.mezame/master.key` holds 32 random bytes, mode `0600`, written once.
//! `load_or_create` writes and syncs them to a temporary sibling opened
//! with `create_new`, then publishes that file onto the final name with a
//! hard link. Two processes that both find no key race to publish, and
//! `link(2)` answering `EEXIST` decides the loser, which unlinks its own
//! sibling and reads the winner's key instead of replacing a key another
//! process may already have sealed with. The final name is never opened
//! with an exclusive create of its own: a racing reader would find that
//! file empty between its creation and its write, where the link publishes
//! a whole file or nothing. Two sub-keys come off the key by HKDF-SHA256,
//! one for credentials and one for the session cookie, so neither use ever
//! sees the other's key.
//!
//! A credential payload is sealed with XChaCha20-Poly1305 under the
//! credential key, a fresh 24-byte nonce per write, and the row id as
//! associated data, so a ciphertext moved to another row fails to open.
//! The key file protects the database when it travels alone; it does not
//! protect a copy of the whole `~/.mezame`, and the documentation says so.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::Sha256;

/// Bytes in the master key and in each derived key.
pub const KEY_LEN: usize = 32;
/// Bytes in an XChaCha20-Poly1305 nonce.
pub const NONCE_LEN: usize = 24;

const CREDENTIAL_INFO: &[u8] = b"mezame credentials v1";
const COOKIE_INFO: &[u8] = b"mezame cookie v1";

/// The 32 bytes at `~/.mezame/master.key`.
pub struct MasterKey([u8; KEY_LEN]);

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// Why the key file could not be read or created. Every variant renders as
/// one line naming the path and what is required.
#[derive(Debug)]
pub enum KeyError {
    /// No file at the path.
    NotFound(PathBuf),
    /// Something other than a regular file: a directory, a symlink.
    NotRegular(PathBuf),
    /// Readable by group or others.
    Loose { path: PathBuf, mode: u32 },
    /// Not exactly 32 bytes.
    WrongLength { path: PathBuf, len: u64 },
    /// The operating system refused.
    Io { path: PathBuf, source: io::Error },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::NotFound(path) => write!(f, "{} does not exist", path.display()),
            KeyError::NotRegular(path) => write!(
                f,
                "{} is not a regular file; the master key must be one, mode 0600, 32 bytes",
                path.display()
            ),
            KeyError::Loose { path, mode } => write!(
                f,
                "{} is readable by others (mode {:o}); the master key must be a regular file, \
                 mode 0600, {KEY_LEN} bytes: run `chmod 600 {}`",
                path.display(),
                mode & 0o777,
                path.display()
            ),
            KeyError::WrongLength { path, len } => write!(
                f,
                "{} holds {len} bytes; the master key must be a regular file, mode 0600, \
                 exactly {KEY_LEN} bytes",
                path.display()
            ),
            KeyError::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for KeyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KeyError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl MasterKey {
    /// Read the key at `path`, refusing anything but a regular file of 32
    /// bytes that only its owner can read. Creates nothing.
    pub fn load(path: &Path) -> Result<Self, KeyError> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(KeyError::NotFound(path.to_path_buf()))
            }
            Err(source) => {
                return Err(KeyError::Io {
                    path: path.to_path_buf(),
                    source,
                })
            }
        };
        if !meta.file_type().is_file() {
            return Err(KeyError::NotRegular(path.to_path_buf()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(KeyError::Loose {
                    path: path.to_path_buf(),
                    mode,
                });
            }
        }
        if meta.len() != KEY_LEN as u64 {
            return Err(KeyError::WrongLength {
                path: path.to_path_buf(),
                len: meta.len(),
            });
        }
        let bytes = fs::read(path).map_err(|source| KeyError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let key: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|b: Vec<u8>| KeyError::WrongLength {
                path: path.to_path_buf(),
                len: b.len() as u64,
            })?;
        Ok(Self(key))
    }

    /// Create the key at `path`, or read the one that exists. The bytes are
    /// written and synced to an exclusive sibling first and published with
    /// a hard link, which fails when the final name exists and never shows
    /// a reader a half-written file; a second creator racing the first
    /// therefore finds a whole key and reads it, and no key is ever
    /// replaced. `rename` would replace an existing key, and an exclusive
    /// open of the final path would let a racing reader see it empty.
    pub fn load_or_create(path: &Path) -> Result<Self, KeyError> {
        if path.exists() {
            return Self::load(path);
        }
        let io_at = |source: io::Error| KeyError::Io {
            path: path.to_path_buf(),
            source,
        };
        let mut bytes = [0u8; KEY_LEN];
        getrandom::getrandom(&mut bytes).expect("OS entropy source");
        let tmp = crate::config::temp_sibling(path).map_err(io_at)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options
            .open(&tmp)
            .and_then(|mut file| file.write_all(&bytes).and_then(|()| file.sync_all()));
        if let Err(source) = written {
            let _ = fs::remove_file(&tmp);
            return Err(io_at(source));
        }
        let published = fs::hard_link(&tmp, path);
        let _ = fs::remove_file(&tmp);
        match published {
            Ok(()) => Ok(Self(bytes)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Self::load(path),
            Err(source) => Err(io_at(source)),
        }
    }

    /// The two sub-keys, by HKDF-SHA256 with no salt and a fixed info
    /// string each.
    pub fn keys(&self) -> Keys {
        let hkdf = Hkdf::<Sha256>::new(None, &self.0);
        let mut credential = [0u8; KEY_LEN];
        let mut cookie = [0u8; KEY_LEN];
        hkdf.expand(CREDENTIAL_INFO, &mut credential)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hkdf.expand(COOKIE_INFO, &mut cookie)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Keys { credential, cookie }
    }

    /// A key from bytes a test chose.
    #[doc(hidden)]
    pub fn from_bytes_for_test(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw bytes, for a test comparing them to the file's.
    #[doc(hidden)]
    pub fn bytes_for_test(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

/// The derived keys: one for credential payloads, one for the cookie MAC.
#[derive(Clone)]
pub struct Keys {
    pub credential: [u8; KEY_LEN],
    pub cookie: [u8; KEY_LEN],
}

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Keys(..)")
    }
}

/// Why a payload could not be opened. One variant on purpose: a moved
/// ciphertext, a changed nonce and a changed byte all read the same, and
/// the cipher's own error text never reaches a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    Tampered,
}

impl fmt::Display for CryptoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the stored credential could not be opened with this master key")
    }
}

impl std::error::Error for CryptoError {}

/// Encrypt `payload` for the row `row_id`: the nonce and the ciphertext,
/// both to be stored beside the row.
pub fn seal(key: &[u8; KEY_LEN], row_id: &str, payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).expect("OS entropy source");
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("a 32-byte key");
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: payload,
                aad: row_id.as_bytes(),
            },
        )
        .expect("encrypting an in-memory buffer cannot fail");
    (nonce.to_vec(), ciphertext)
}

/// Decrypt what [`seal`] produced for the same row, or `Tampered`.
pub fn open(
    key: &[u8; KEY_LEN],
    row_id: &str,
    nonce: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| CryptoError::Tampered)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("a 32-byte key");
    cipher
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad: row_id.as_bytes(),
            },
        )
        .map_err(|_| CryptoError::Tampered)
}
