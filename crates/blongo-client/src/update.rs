//! Update checks against a signed manifest. Nothing is installed: an
//! available update can be downloaded and its SHA-256 verified, and the
//! user installs it.
//!
//! The manifest endpoint serves
//! `{"manifest": BASE64(JSON), "signature": BASE64(Ed25519 signature of
//! those JSON bytes)}`, where the JSON is
//! `{"version": "0.2.0", "platform": "linux-x86_64", "expires": UNIX_SECS,
//! "url": "https://…", "sha256": "…", "notes": "…"}`.
//! Only a manifest signed by the release key is believed; the URL and
//! hash come from inside the signed bytes, so a compromised download host
//! cannot hand out another file. The signed platform keeps one OS's build
//! from being offered to another, and the expiry keeps an old (signed but
//! superseded) manifest from being replayed forever.
//!
//! This build has no release key compiled in ([`RELEASE_KEY`] is `None`):
//! update checks need `BLONGO_UPDATE_URL` and `BLONGO_UPDATE_KEY`
//! (base64 public key), which is how the tests use a local fake server.
//! Once a build has a release key, `BLONGO_UPDATE_KEY` is ignored: an
//! environment variable can never replace the key a release trusts.

use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http::{self, Request};

/// The release signing key (32 bytes), when this build has one.
pub const RELEASE_KEY: Option<[u8; 32]> = None;
/// Largest download accepted.
pub const MAX_DOWNLOAD: u64 = 512 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    /// `<os>-<arch>` as [`platform`] gives it.
    pub platform: String,
    /// The manifest is not believed after this time (Unix seconds).
    pub expires: i64,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    UpToDate { latest: String },
    Available(Manifest),
}

/// This build's platform as manifests name it (`linux-x86_64`, …).
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Where to look and whom to trust: the compiled-in key (always, when the
/// build has one) or, in builds without one, `BLONGO_UPDATE_KEY`; the URL
/// from `BLONGO_UPDATE_URL`.
pub fn configured() -> Option<(String, VerifyingKey)> {
    let url = std::env::var("BLONGO_UPDATE_URL")
        .ok()
        .filter(|u| !u.is_empty())?;
    let key = trusted_key(RELEASE_KEY, std::env::var("BLONGO_UPDATE_KEY").ok())?;
    Some((url, VerifyingKey::from_bytes(&key).ok()?))
}

/// The release key wins over the environment's.
fn trusted_key(release: Option<[u8; 32]>, from_env: Option<String>) -> Option<[u8; 32]> {
    match release {
        Some(key) => Some(key),
        None => base64::engine::general_purpose::STANDARD
            .decode(from_env?.trim())
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok()),
    }
}

#[derive(Deserialize)]
struct Envelope {
    manifest: String,
    signature: String,
}

/// Verify an envelope's signature and parse its manifest, which must be
/// for this platform and not expired.
pub fn verify(envelope: &[u8], key: &VerifyingKey) -> Result<Manifest, String> {
    verify_at(envelope, key, &platform(), crate::secret::unix_now() as i64)
}

fn verify_at(
    envelope: &[u8],
    key: &VerifyingKey,
    platform: &str,
    now: i64,
) -> Result<Manifest, String> {
    let env: Envelope =
        serde_json::from_slice(envelope).map_err(|e| format!("bad update manifest: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let bytes = b64
        .decode(env.manifest.trim())
        .map_err(|_| "bad update manifest encoding")?;
    let sig = b64
        .decode(env.signature.trim())
        .ok()
        .and_then(|s| <[u8; 64]>::try_from(s).ok())
        .ok_or("bad update signature encoding")?;
    key.verify(&bytes, &Signature::from_bytes(&sig))
        .map_err(|_| "the update manifest is not signed by the release key".to_owned())?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("bad update manifest: {e}"))?;
    if manifest.platform != platform {
        return Err(format!(
            "the update manifest is for {}, not {platform}",
            manifest.platform
        ));
    }
    if manifest.expires <= now {
        return Err("the update manifest has expired".into());
    }
    http::check_url(&manifest.url)?;
    if manifest.sha256.len() != 64 || !manifest.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("bad sha256 in the update manifest".into());
    }
    Ok(manifest)
}

/// `a > b` for dotted numeric versions (`0.10.1` > `0.9`); a
/// pre-release suffix (`-rc1`) sorts before the release.
pub fn newer(a: &str, b: &str) -> bool {
    fn parts(v: &str) -> (Vec<u64>, bool) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once('-') {
            Some((c, _)) => (c, true),
            None => (v, false),
        };
        (
            core.split('.').map(|p| p.parse().unwrap_or(0)).collect(),
            pre,
        )
    }
    let (mut pa, prea) = parts(a);
    let (mut pb, preb) = parts(b);
    let n = pa.len().max(pb.len());
    pa.resize(n, 0);
    pb.resize(n, 0);
    match pa.cmp(&pb) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => preb && !prea,
    }
}

/// Fetch and verify the manifest; compare with `current`.
pub async fn check(url: &str, key: &VerifyingKey, current: &str) -> Result<Status, String> {
    let resp = http::send(Request::get(url)).await?;
    if !resp.ok() {
        return Err(format!("update check: HTTP {}", resp.status));
    }
    let manifest = verify(&resp.body, key)?;
    Ok(if newer(&manifest.version, current) {
        Status::Available(manifest)
    } else {
        Status::UpToDate {
            latest: manifest.version,
        }
    })
}

/// Download the update into `dir` and check its SHA-256 against the
/// signed manifest; a mismatch deletes the file.
pub async fn download(
    manifest: &Manifest,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    crate::secret::private_dir(dir).map_err(|e| e.to_string())?;
    let name = manifest
        .url
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty() && !n.contains(['?', '#']) && *n != "." && *n != "..")
        .unwrap_or("blongo-update");
    let dest = dir.join(name);
    http::download(&manifest.url, &dest, MAX_DOWNLOAD).await?;
    let file = dest.clone();
    let digest = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
        use std::io::Read;
        let mut f = std::fs::File::open(&file)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    if !digest.eq_ignore_ascii_case(&manifest.sha256) {
        let _ = std::fs::remove_file(&dest);
        return Err("the downloaded update does not match the signed checksum".into());
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    pub(crate) fn signed(key: &SigningKey, manifest: &Manifest) -> Vec<u8> {
        let bytes = serde_json::to_vec(manifest).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD;
        serde_json::json!({
            "manifest": b64.encode(&bytes),
            "signature": b64.encode(key.sign(&bytes).to_bytes()),
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("1.0", "0.99"));
        assert!(!newer("0.1.0", "0.1"));
        assert!(newer("0.2.0", "0.2.0-rc1"));
        assert!(!newer("0.2.0-rc1", "0.2.0"));
        assert!(newer("v1.2.3", "1.2.2"));
    }

    #[test]
    fn only_manifests_signed_by_the_key_are_believed() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let other = SigningKey::from_bytes(&[8u8; 32]);
        let m = Manifest {
            version: "9.9.9".into(),
            platform: platform(),
            expires: crate::secret::unix_now() as i64 + 3600,
            url: "https://example.invalid/blongo.tar.gz".into(),
            sha256: "a".repeat(64),
            notes: String::new(),
        };
        assert_eq!(verify(&signed(&key, &m), &key.verifying_key()).unwrap(), m);
        assert!(verify(&signed(&other, &m), &key.verifying_key()).is_err());
        // A tampered manifest under a valid signature fails too.
        let mut env: serde_json::Value = serde_json::from_slice(&signed(&key, &m)).unwrap();
        let tampered = Manifest {
            url: "https://evil.invalid/x".into(),
            ..m.clone()
        };
        env["manifest"] = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&tampered).unwrap())
            .into();
        assert!(verify(env.to_string().as_bytes(), &key.verifying_key()).is_err());
        // Plain http off loopback is refused even when signed.
        let insecure = Manifest {
            url: "http://example.invalid/x".into(),
            ..m
        };
        assert!(verify(&signed(&key, &insecure), &key.verifying_key()).is_err());
    }

    #[test]
    fn manifests_are_bound_to_a_platform_and_expire() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let m = Manifest {
            version: "9.9.9".into(),
            platform: "linux-x86_64".into(),
            expires: 1_000,
            url: "https://example.invalid/blongo.tar.gz".into(),
            sha256: "a".repeat(64),
            notes: String::new(),
        };
        let env = signed(&key, &m);
        let k = key.verifying_key();
        assert!(verify_at(&env, &k, "linux-x86_64", 999).is_ok());
        let err = verify_at(&env, &k, "linux-x86_64", 1_000).unwrap_err();
        assert!(err.contains("expired"), "{err}");
        let err = verify_at(&env, &k, "windows-x86_64", 999).unwrap_err();
        assert!(err.contains("not windows-x86_64"), "{err}");
    }

    #[test]
    fn a_release_key_cannot_be_replaced_from_the_environment() {
        let env_key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        assert_eq!(
            trusted_key(Some([1; 32]), Some(env_key.clone())),
            Some([1; 32])
        );
        assert_eq!(trusted_key(None, Some(env_key)), Some([9; 32]));
        assert_eq!(trusted_key(None, None), None);
    }
}
