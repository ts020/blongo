//! Who may connect: paired devices and one-time pairing codes.
//!
//! Everything lives in the server's private state directory
//! (`<data_dir>/server`, 0700), in owner-only files (0600):
//!
//! - `identity.json`: the server id clients pin at pairing.
//! - `devices.json`: paired devices — name, Ed25519 public key, and the
//!   SHA-256 of the bearer token (the token itself is never stored).
//! - `pairing.json`: outstanding pairing codes, hashed, with expiry. `blongo-
//!   serve pair` adds one (even while a server runs); a successful pairing
//!   consumes it. While a code is outstanding its stored hash is as good as
//!   the code itself: the code space is small enough to search, so anyone
//!   who can read the file can pair until the code expires or is used. The
//!   file is owner-only (0600) for that reason; the hash only keeps the
//!   code out of plain sight (backups, logs of file contents).
//!
//! Read-modify-write cycles take an exclusive lock on `auth.lock`, so the
//! `pair` command and a running server never lose each other's writes.
//! Secrets are compared in constant time and never logged.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use blongo_client::secret::{
    ProofError, b64, ct_eq, new_token, private_dir, random, sha256, unb64, unix_now, verify_proof,
    write_private,
};
use blongo_protocol::wire::{PROOF_REPLAY_WINDOW_SECS, Proof, ProofPurpose};
use serde::{Deserialize, Serialize};

/// Pairing codes: unambiguous characters, 10 of them (~49 bits).
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTVWXYZ23456789";
const CODE_LEN: usize = 10;
const MAX_CODES: usize = 16;
/// More failed pairings than this within [`FAILURE_WINDOW_SECS`] wipes
/// every outstanding code.
const MAX_PAIR_FAILURES: usize = 10;
const FAILURE_WINDOW_SECS: u64 = 600;
const MAX_REMEMBERED_PROOFS: usize = 16_384;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Identity {
    server_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// URL-safe base64 of the Ed25519 public key.
    pub public_key: String,
    /// URL-safe base64 of SHA-256(token).
    token_sha256: String,
    pub created_at: u64,
    pub last_seen_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Devices {
    devices: Vec<Device>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PairingCode {
    code_sha256: String,
    expires_at: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct PairingCodes {
    codes: Vec<PairingCode>,
}

/// Why authentication failed. Logged by the server; the client only hears
/// "unauthorized".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    UnknownDevice,
    BadProof(ProofError),
    ReplayedProof,
    BadCode,
    KeyMismatch,
    Storage(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDevice => write!(f, "unknown device"),
            Self::BadProof(e) => write!(f, "bad proof ({e:?})"),
            Self::ReplayedProof => write!(f, "replayed proof"),
            Self::BadCode => write!(f, "unknown, used or expired pairing code"),
            Self::KeyMismatch => write!(f, "malformed device key"),
            Self::Storage(e) => write!(f, "auth storage: {e}"),
        }
    }
}

/// Recently seen proof ids (defence in depth: every proof also signs the
/// connection's fresh nonce, so a replay fails the signature anyway).
#[derive(Default)]
struct ReplayCache {
    seen: HashMap<Vec<u8>, u64>,
    order: VecDeque<(Vec<u8>, u64)>,
}

impl ReplayCache {
    /// `false`: this jti was seen within the proof lifetime.
    fn insert(&mut self, jti: &[u8], now: u64) -> bool {
        while let Some((_, at)) = self.order.front() {
            if now.saturating_sub(*at) > PROOF_REPLAY_WINDOW_SECS
                || self.order.len() >= MAX_REMEMBERED_PROOFS
            {
                let (old, _) = self.order.pop_front().expect("front");
                self.seen.remove(&old);
            } else {
                break;
            }
        }
        if self.seen.contains_key(jti) {
            return false;
        }
        self.seen.insert(jti.to_vec(), now);
        self.order.push_back((jti.to_vec(), now));
        true
    }
}

pub struct AuthStore {
    dir: PathBuf,
    server_id: String,
    replay: Mutex<ReplayCache>,
    failures: Mutex<VecDeque<u64>>,
    /// Clock override (tests).
    now: Option<Box<dyn Fn() -> u64 + Send + Sync>>,
}

impl AuthStore {
    /// Open (creating) the state directory and the server identity.
    pub fn open(dir: &Path) -> std::io::Result<Self> {
        private_dir(dir)?;
        let identity_path = dir.join("identity.json");
        let server_id = match std::fs::read(&identity_path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Identity>(&b).ok())
        {
            Some(identity) => identity.server_id,
            None => {
                let identity = Identity {
                    server_id: b64(&random::<16>()),
                };
                write_private(
                    &identity_path,
                    &serde_json::to_vec_pretty(&identity).expect("json"),
                )?;
                identity.server_id
            }
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            server_id,
            replay: Mutex::default(),
            failures: Mutex::default(),
            now: None,
        })
    }

    #[doc(hidden)]
    pub fn with_clock(mut self, now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now = Some(Box::new(now));
        self
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    fn now(&self) -> u64 {
        self.now.as_ref().map_or_else(unix_now, |f| f())
    }

    fn locked<T>(&self, f: impl FnOnce() -> Result<T, AuthError>) -> Result<T, AuthError> {
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("auth.lock"))
            .map_err(|e| AuthError::Storage(e.to_string()))?;
        lock.lock().map_err(|e| AuthError::Storage(e.to_string()))?;
        let out = f();
        let _ = lock.unlock();
        out
    }

    fn read<T: for<'de> Deserialize<'de> + Default>(&self, name: &str) -> Result<T, AuthError> {
        match std::fs::read(self.dir.join(name)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| AuthError::Storage(format!("{name}: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(AuthError::Storage(format!("{name}: {e}"))),
        }
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), AuthError> {
        write_private(
            &self.dir.join(name),
            &serde_json::to_vec_pretty(value).expect("json"),
        )
        .map_err(|e| AuthError::Storage(format!("{name}: {e}")))
    }

    /// Create a one-time pairing code valid for `ttl_secs`. Only its hash
    /// is stored (equivalent to the code while it is outstanding, see the
    /// module docs); the returned text is shown once.
    pub fn add_pairing_code(&self, ttl_secs: u64) -> Result<String, AuthError> {
        // Rejection sampling: only bytes below the largest multiple of the
        // alphabet size, so every character is equally likely.
        let limit = 256 - 256 % CODE_ALPHABET.len();
        let mut code = String::with_capacity(CODE_LEN);
        while code.len() < CODE_LEN {
            for b in random::<16>() {
                if (b as usize) < limit && code.len() < CODE_LEN {
                    code.push(CODE_ALPHABET[b as usize % CODE_ALPHABET.len()] as char);
                }
            }
        }
        let now = self.now();
        self.locked(|| {
            let mut codes: PairingCodes = self.read("pairing.json")?;
            codes.codes.retain(|c| c.expires_at > now);
            while codes.codes.len() >= MAX_CODES {
                codes.codes.remove(0);
            }
            codes.codes.push(PairingCode {
                code_sha256: b64(&sha256(code.as_bytes())),
                expires_at: now + ttl_secs,
            });
            self.write("pairing.json", &codes)
        })?;
        Ok(format!("{}-{}", &code[..5], &code[5..]))
    }

    /// Outstanding (unexpired) pairing codes.
    pub fn pending_codes(&self) -> usize {
        let now = self.now();
        self.read::<PairingCodes>("pairing.json")
            .map(|c| c.codes.iter().filter(|c| c.expires_at > now).count())
            .unwrap_or(0)
    }

    fn check_replay(&self, proof: &Proof, now: u64) -> Result<(), AuthError> {
        if self
            .replay
            .lock()
            .expect("replay cache")
            .insert(&proof.jti, now)
        {
            Ok(())
        } else {
            Err(AuthError::ReplayedProof)
        }
    }

    /// Authenticate a paired device: a proof made with its bound key over
    /// the SHA-256 of its token, which is all the server keeps of it.
    pub fn verify_token(
        &self,
        device_id: &str,
        proof: &Proof,
        nonce: &[u8],
    ) -> Result<Device, AuthError> {
        let now = self.now();
        let devices: Devices = self.read("devices.json")?;
        let device = devices
            .devices
            .iter()
            .find(|d| ct_eq(d.id.as_bytes(), device_id.as_bytes()))
            .ok_or(AuthError::UnknownDevice)?
            .clone();
        let stored = unb64(&device.token_sha256).ok_or(AuthError::KeyMismatch)?;
        let public_key = unb64(&device.public_key).ok_or(AuthError::KeyMismatch)?;
        verify_proof(
            proof,
            &public_key,
            ProofPurpose::Token,
            &self.server_id,
            nonce,
            &stored,
        )
        .map_err(AuthError::BadProof)?;
        self.check_replay(proof, now)?;
        // Best effort: when it was last used.
        let _ = self.locked(|| {
            let mut devices: Devices = self.read("devices.json")?;
            if let Some(d) = devices.devices.iter_mut().find(|d| d.id == device.id) {
                d.last_seen_at = now;
            }
            self.write("devices.json", &devices)
        });
        Ok(device)
    }

    /// Trade a pairing code for a device credential bound to `public_key`.
    /// The code itself is not presented: the proof signs its SHA-256, and
    /// the server tries the proof against every outstanding code. The code
    /// is consumed on success. Returns the device and its token.
    pub fn pair(
        &self,
        device_name: &str,
        public_key: &[u8],
        proof: &Proof,
        nonce: &[u8],
    ) -> Result<(Device, String), AuthError> {
        let now = self.now();
        if public_key.len() != 32 {
            return Err(AuthError::KeyMismatch);
        }
        let result = self.locked(|| {
            let mut codes: PairingCodes = self.read("pairing.json")?;
            codes.codes.retain(|c| c.expires_at > now);
            // Every code is tried (no early exit on a match).
            let mut found = None;
            for (i, c) in codes.codes.iter().enumerate() {
                let hash = unb64(&c.code_sha256).unwrap_or_default();
                if verify_proof(
                    proof,
                    public_key,
                    ProofPurpose::Pair,
                    &self.server_id,
                    nonce,
                    &hash,
                )
                .is_ok()
                {
                    found = Some(i);
                }
            }
            let Some(index) = found else {
                return Err(AuthError::BadCode);
            };
            self.check_replay(proof, now)?;
            codes.codes.remove(index);
            self.write("pairing.json", &codes)?;
            let token = new_token();
            let device = Device {
                id: blongo_protocol::Uuid::now_v7().to_string(),
                name: device_name.chars().take(64).collect(),
                public_key: b64(public_key),
                token_sha256: b64(&sha256(token.as_bytes())),
                created_at: now,
                last_seen_at: now,
            };
            let mut devices: Devices = self.read("devices.json")?;
            devices.devices.push(device.clone());
            self.write("devices.json", &devices)?;
            Ok((device, token))
        });
        if matches!(result, Err(AuthError::BadCode)) {
            self.pair_failed(now);
        }
        result
    }

    /// Too many failed pairings in a short time: drop every outstanding
    /// code (an online guesser then has nothing left to find).
    fn pair_failed(&self, now: u64) {
        let mut failures = self.failures.lock().expect("failures");
        failures.retain(|t| now.saturating_sub(*t) < FAILURE_WINDOW_SECS);
        failures.push_back(now);
        if failures.len() > MAX_PAIR_FAILURES {
            failures.clear();
            let _ = self.locked(|| self.write("pairing.json", &PairingCodes::default()));
            eprintln!(
                "blongo-serve: too many failed pairing attempts; every outstanding pairing code was revoked"
            );
        }
    }

    pub fn devices(&self) -> Vec<Device> {
        self.read::<Devices>("devices.json")
            .map(|d| d.devices)
            .unwrap_or_default()
    }

    /// Remove devices by id or name; returns the ids removed. New
    /// connections with their tokens fail from now on; a running server
    /// also closes their live connections when asked through its socket
    /// (`ClientMsg::Revoke`).
    pub fn revoke(&self, id_or_name: &str) -> Result<Vec<String>, AuthError> {
        self.locked(|| {
            let mut devices: Devices = self.read("devices.json")?;
            let (gone, kept): (Vec<Device>, Vec<Device>) = devices
                .devices
                .into_iter()
                .partition(|d| d.id == id_or_name || d.name == id_or_name);
            devices.devices = kept;
            self.write("devices.json", &devices)?;
            Ok(gone.into_iter().map(|d| d.id).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use blongo_client::handshake::normalize_code;
    use blongo_client::secret::{make_proof, new_device_key};

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("blongo-auth-{}", b64(&random::<6>())))
    }

    fn pair_new(
        store: &AuthStore,
        code: &str,
    ) -> Result<(Device, String, ed25519_dalek::SigningKey), AuthError> {
        let key = new_device_key();
        let nonce = random::<32>();
        let proof = make_proof(
            &key,
            ProofPurpose::Pair,
            store.server_id(),
            &nonce,
            &normalize_code(code),
        );
        store
            .pair("laptop", &key.verifying_key().to_bytes(), &proof, &nonce)
            .map(|(d, t)| (d, t, key))
    }

    #[test]
    fn pairing_issues_a_bound_token_and_codes_are_single_use() {
        let dir = temp();
        let store = AuthStore::open(&dir).unwrap();
        let code = store.add_pairing_code(600).unwrap();
        assert_eq!(code.len(), 11);
        assert!(
            code.chars()
                .all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8)))
        );
        // Case and separators do not matter.
        let typed = code.to_lowercase().replace('-', " ");
        let (device, token, key) = pair_new(&store, &typed).unwrap();
        assert_eq!(device.name, "laptop");
        // Used once: gone.
        assert_eq!(pair_new(&store, &code).unwrap_err(), AuthError::BadCode);

        let nonce = random::<32>();
        let proof = make_proof(&key, ProofPurpose::Token, store.server_id(), &nonce, &token);
        assert!(store.verify_token(&device.id, &proof, &nonce).is_ok());

        // Same proof again (replay): refused even on the same nonce.
        assert_eq!(
            store.verify_token(&device.id, &proof, &nonce).unwrap_err(),
            AuthError::ReplayedProof
        );
        // A proof for another connection's nonce fails the signature.
        let proof = make_proof(&key, ProofPurpose::Token, store.server_id(), &nonce, &token);
        assert!(matches!(
            store.verify_token(&device.id, &proof, &random::<32>()),
            Err(AuthError::BadProof(ProofError::BadSignature))
        ));
        // A stolen token without the key.
        let thief = new_device_key();
        let proof = make_proof(
            &thief,
            ProofPurpose::Token,
            store.server_id(),
            &nonce,
            &token,
        );
        assert!(matches!(
            store.verify_token(&device.id, &proof, &nonce),
            Err(AuthError::BadProof(ProofError::BadSignature))
        ));
        // The key without the token.
        let proof = make_proof(&key, ProofPurpose::Token, store.server_id(), &nonce, "nope");
        assert!(matches!(
            store.verify_token(&device.id, &proof, &nonce),
            Err(AuthError::BadProof(ProofError::BadSignature))
        ));
        assert_eq!(
            store.verify_token("someone", &proof, &nonce).unwrap_err(),
            AuthError::UnknownDevice
        );

        // The token is never stored, only its hash; files are owner-only.
        let devices = std::fs::read_to_string(dir.join("devices.json")).unwrap();
        assert!(!devices.contains(&token));
        let pairing = std::fs::read_to_string(dir.join("pairing.json")).unwrap();
        assert!(!pairing.contains(&code.replace('-', "")));
        #[cfg(unix)]
        for name in ["devices.json", "pairing.json", "identity.json"] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "{name}");
        }

        // Revoked: the token stops working.
        assert_eq!(store.revoke("laptop").unwrap(), vec![device.id.clone()]);
        assert!(store.revoke("laptop").unwrap().is_empty());
        let proof = make_proof(&key, ProofPurpose::Token, store.server_id(), &nonce, &token);
        assert_eq!(
            store.verify_token(&device.id, &proof, &nonce).unwrap_err(),
            AuthError::UnknownDevice
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expired_codes_fail_and_client_clock_skew_does_not_matter() {
        let dir = temp();
        let clock = Arc::new(AtomicU64::new(unix_now()));
        let c = clock.clone();
        let store = AuthStore::open(&dir)
            .unwrap()
            .with_clock(move || c.load(Ordering::SeqCst));
        let code = store.add_pairing_code(60).unwrap();
        clock.fetch_add(61, Ordering::SeqCst);
        assert_eq!(pair_new(&store, &code).unwrap_err(), AuthError::BadCode);
        assert_eq!(store.pending_codes(), 0);

        // A client whose clock is an hour off still pairs and connects:
        // the per-connection nonce makes proofs fresh, not `iat`.
        let code = store.add_pairing_code(3600).unwrap();
        clock.fetch_add(3600, Ordering::SeqCst);
        let code2 = store.add_pairing_code(3600).unwrap();
        let (device, token, key) = pair_new(&store, &code2).unwrap();
        let nonce = random::<32>();
        let proof = make_proof(&key, ProofPurpose::Token, store.server_id(), &nonce, &token);
        assert!(store.verify_token(&device.id, &proof, &nonce).is_ok());
        // The first code expired meanwhile (server clock).
        assert_eq!(pair_new(&store, &code).unwrap_err(), AuthError::BadCode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn guessing_revokes_every_code() {
        let dir = temp();
        let store = AuthStore::open(&dir).unwrap();
        let code = store.add_pairing_code(600).unwrap();
        for _ in 0..=MAX_PAIR_FAILURES {
            assert_eq!(
                pair_new(&store, "AAAAA-AAAAA").unwrap_err(),
                AuthError::BadCode
            );
        }
        assert_eq!(store.pending_codes(), 0);
        assert_eq!(pair_new(&store, &code).unwrap_err(), AuthError::BadCode);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn code_characters_are_uniform() {
        let dir = temp();
        let store = AuthStore::open(&dir).unwrap();
        let mut counts = [0usize; 30];
        for _ in 0..400 {
            for c in store
                .add_pairing_code(600)
                .unwrap()
                .chars()
                .filter(|c| *c != '-')
            {
                counts[CODE_ALPHABET.iter().position(|a| *a as char == c).unwrap()] += 1;
            }
        }
        // 4000 draws over 30 characters: ~133 each. Modulo bias would
        // give the first 16 characters ~9% more; allow wide noise only.
        assert!(counts.iter().all(|&n| (70..200).contains(&n)), "{counts:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn identity_is_stable() {
        let dir = temp();
        let a = AuthStore::open(&dir).unwrap().server_id().to_owned();
        let b = AuthStore::open(&dir).unwrap().server_id().to_owned();
        assert_eq!(a, b);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
