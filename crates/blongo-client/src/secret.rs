//! Secrets on disk and in memory: owner-only files, random tokens, device
//! keys and the proof of possession. Shared by the client and the server.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use blongo_protocol::wire::{Proof, ProofPurpose, proof_message, secret_sha256};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub use blongo_forge::fs::{private_dir, write_private};

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unb64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim()).ok()
}

/// Bytes from the operating system's CSPRNG.
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("operating system randomness");
    out
}

/// A new bearer token (256 bits, URL-safe base64).
pub fn new_token() -> String {
    b64(&random::<32>())
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// Constant-time equality of two byte strings (length may leak).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// A device key (Ed25519).
pub fn new_device_key() -> SigningKey {
    SigningKey::from_bytes(&random::<32>())
}

pub fn device_key_from_b64(s: &str) -> Option<SigningKey> {
    let bytes: [u8; 32] = unb64(s)?.try_into().ok()?;
    Some(SigningKey::from_bytes(&bytes))
}

/// Unix seconds.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Make a proof for this connection's challenge.
pub fn make_proof(
    key: &SigningKey,
    purpose: ProofPurpose,
    server_id: &str,
    nonce: &[u8],
    secret: &str,
) -> Proof {
    let iat = unix_now();
    let jti = random::<16>();
    let public = key.verifying_key().to_bytes();
    let message = proof_message(
        purpose,
        server_id,
        nonce,
        iat,
        &jti,
        &secret_sha256(secret),
        &public,
    );
    Proof {
        iat,
        jti: jti.to_vec(),
        signature: key.sign(&message).to_bytes().to_vec(),
    }
}

/// Why a proof was refused (for the server's log; the client only hears
/// "unauthorized").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofError {
    Malformed,
    BadSignature,
}

/// Check a proof's shape and signature against `public_key`, for the
/// secret whose SHA-256 the server stored. Freshness comes from the
/// connection's nonce (`iat` is signed but not compared with this clock);
/// replay of the `jti` is the caller's business (it keeps the cache).
pub fn verify_proof(
    proof: &Proof,
    public_key: &[u8],
    purpose: ProofPurpose,
    server_id: &str,
    nonce: &[u8],
    secret_sha256: &[u8],
) -> Result<(), ProofError> {
    let key: [u8; 32] = public_key.try_into().map_err(|_| ProofError::Malformed)?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| ProofError::Malformed)?;
    let signature: [u8; 64] = proof
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| ProofError::Malformed)?;
    if proof.jti.len() != 16 {
        return Err(ProofError::Malformed);
    }
    let message = proof_message(
        purpose,
        server_id,
        nonce,
        proof.iat,
        &proof.jti,
        secret_sha256,
        public_key,
    );
    key.verify(&message, &Signature::from_bytes(&signature))
        .map_err(|_| ProofError::BadSignature)?;
    // Reject weak (small-order) keys and malleated signatures.
    key.verify_strict(&message, &Signature::from_bytes(&signature))
        .map_err(|_| ProofError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Never chmod a directory Blongo does not own or that is shared:
    /// writing a private file into an existing directory leaves its mode
    /// alone, and a sticky (shared, `/tmp`-like) directory is refused.
    #[cfg(unix)]
    #[test]
    fn existing_and_shared_directories_keep_their_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let root = std::env::temp_dir().join(format!("blongo-secret-{}", b64(&random::<6>())));
        let open_dir = root.join("open");
        let shared = root.join("shared");
        std::fs::create_dir_all(&open_dir).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();

        write_private(&open_dir.join("a"), b"x").unwrap();
        assert_eq!(mode(&open_dir), 0o755);
        assert_eq!(mode(&open_dir.join("a")), 0o600);
        write_private(&shared.join("b"), b"x").unwrap();
        assert_eq!(mode(&shared), 0o1777);
        assert!(private_dir(&shared).is_err());
        assert_eq!(mode(&shared), 0o1777);
        // A missing directory is created private.
        write_private(&root.join("new/c"), b"x").unwrap();
        assert_eq!(mode(&root.join("new")), 0o700);
        // One of ours that is too open is tightened.
        private_dir(&open_dir).unwrap();
        assert_eq!(mode(&open_dir), 0o700);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn proofs_verify_and_fail_on_any_change() {
        let key = new_device_key();
        let public = key.verifying_key().to_bytes();
        let nonce = random::<32>();
        let proof = make_proof(&key, ProofPurpose::Token, "srv", &nonce, "tok");
        let now = ();
        let check = |p: &Proof, purpose, server: &str, nonce: &[u8], secret: &str, _now: ()| {
            verify_proof(p, &public, purpose, server, nonce, &secret_sha256(secret))
        };
        assert_eq!(
            check(&proof, ProofPurpose::Token, "srv", &nonce, "tok", now),
            Ok(())
        );
        assert_eq!(
            check(&proof, ProofPurpose::Pair, "srv", &nonce, "tok", now),
            Err(ProofError::BadSignature)
        );
        assert_eq!(
            check(&proof, ProofPurpose::Token, "other", &nonce, "tok", now),
            Err(ProofError::BadSignature)
        );
        assert_eq!(
            check(&proof, ProofPurpose::Token, "srv", &[0; 32], "tok", now),
            Err(ProofError::BadSignature)
        );
        assert_eq!(
            check(&proof, ProofPurpose::Token, "srv", &nonce, "tok2", now),
            Err(ProofError::BadSignature)
        );
        // A changed timestamp breaks the signature.
        let mut late = proof.clone();
        late.iat += 1;
        assert_eq!(
            check(&late, ProofPurpose::Token, "srv", &nonce, "tok", now),
            Err(ProofError::BadSignature)
        );
        // Another key's signature.
        let other = make_proof(&new_device_key(), ProofPurpose::Token, "srv", &nonce, "tok");
        assert_eq!(
            check(&other, ProofPurpose::Token, "srv", &nonce, "tok", now),
            Err(ProofError::BadSignature)
        );
        let mut bad = proof.clone();
        bad.signature.pop();
        assert_eq!(
            check(&bad, ProofPurpose::Token, "srv", &nonce, "tok", now),
            Err(ProofError::Malformed)
        );
    }

    #[test]
    fn private_files_are_owner_only_and_atomic() {
        let dir = std::env::temp_dir().join(format!("blongo-secret-{}", b64(&random::<6>())));
        let file = dir.join("sub/creds.json");
        write_private(&file, b"one").unwrap();
        write_private(&file, b"two").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let mode = std::fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
        // No temporary files left behind.
        assert_eq!(
            std::fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert_eq!(new_token().len(), 43);
        assert_ne!(new_token(), new_token());
    }
}
