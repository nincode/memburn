//! Signed release manifest: what the live-view server offers the USB stick as an update.

use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Ed25519 public key of the release signing key, hex; a build can swap it with `MEMBURN_RELEASE_KEY`.
pub const RELEASE_KEY: &str = match option_env!("MEMBURN_RELEASE_KEY") {
    Some(k) if !k.is_empty() => k,
    _ => "422ffd29e1aec4d5e56183e5781eab69eafc4a15445538adfce7c3ef62896753",
};

/// Stick files an update never writes: settings, logs, history and crash logs.
const USER_FILES: [&str; 3] = ["memburn.cfg", "memburn.log", "memburn-history.txt"];

/// `GET /memburn/api/latest`: the manifest's exact JSON text and its signature.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Envelope {
    pub manifest: String,
    /// Ed25519 over the bytes of `manifest`, hex.
    pub sig: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub version: String,
    /// The stick's files as one ustar archive.
    pub tar: Download,
    /// Every file the update writes, with its path from the stick's root.
    pub files: Vec<StickFile>,
    /// The release's download files, for the landing page.
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Download {
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StickFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Asset {
    pub name: String,
    pub size: u64,
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for (k, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * k..2 * k + 2], 16).ok()?;
    }
    Some(out)
}

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// "0.5.0" -> (0, 5, 0).
pub fn version(s: &str) -> Option<(u64, u64, u64)> {
    let mut it = s.split('.').map(|p| p.parse::<u64>().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// `a` is a later version than `b`; false when either does not parse.
pub fn newer(a: &str, b: &str) -> bool {
    matches!((version(a), version(b)), (Some(a), Some(b)) if a > b)
}

/// A relative path an update may write: under `EFI/` or `windows/`, or a known root file; never a user file.
pub fn writable(path: &str) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    let name = parts.last().copied().unwrap_or("");
    let clean = !path.contains('\\') && parts.iter().all(|p| !p.is_empty() && *p != "." && *p != "..");
    let user = USER_FILES.contains(&name) || name.starts_with("memburn-crash-") || name.ends_with(".new");
    let place = matches!(parts[0], "EFI" | "windows") && parts.len() > 1
        || matches!(path, "README.txt" | "memburn.cfg.example");
    clean && !user && place
}

/// Checks the signature with `key_hex`, then parses and sanity-checks the manifest.
pub fn verify(env: &Envelope, key_hex: &str) -> Result<Manifest, String> {
    let key = unhex::<32>(key_hex).ok_or("bad key")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "bad key")?;
    let sig = unhex::<64>(&env.sig).ok_or("bad signature")?;
    key.verify_strict(env.manifest.as_bytes(), &Signature::from_bytes(&sig)).map_err(|_| "bad signature")?;
    let m: Manifest = serde_json::from_str(&env.manifest).map_err(|e| format!("bad manifest: {e}"))?;
    let hash = |h: &str| unhex::<32>(h).is_some();
    if version(&m.version).is_none() {
        return Err(format!("bad manifest version {}", m.version));
    }
    if !hash(&m.tar.sha256) || !(m.tar.url.starts_with("https://") || m.tar.url.starts_with("http://")) {
        return Err("bad manifest download".into());
    }
    if let Some(f) = m.files.iter().find(|f| !writable(&f.path) || !hash(&f.sha256)) {
        return Err(format!("bad manifest file {}", f.path));
    }
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn key() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[7; 32]);
        let pk = sk.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        (sk, pk)
    }

    fn manifest() -> Manifest {
        let h = sha256_hex(b"x");
        Manifest {
            version: "0.5.1".into(),
            tar: Download { url: "https://example.org/s.tar".into(), size: 10, sha256: h.clone() },
            files: vec![StickFile { path: "EFI/memburn/0.5.1/vmlinuz".into(), size: 1, sha256: h }],
            assets: vec![],
        }
    }

    fn sign(sk: &SigningKey, text: &str) -> Envelope {
        let sig = sk.sign(text.as_bytes()).to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        Envelope { manifest: text.into(), sig }
    }

    #[test]
    fn signature_checks() {
        let (sk, pk) = key();
        let text = serde_json::to_string(&manifest()).unwrap();
        let env = sign(&sk, &text);
        assert_eq!(verify(&env, &pk), Ok(manifest()));
        let other = SigningKey::from_bytes(&[8; 32]);
        let other_pk: String = other.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(verify(&env, &other_pk), Err("bad signature".into()));
        let tampered = Envelope { manifest: text.replace("0.5.1", "0.5.2"), ..env.clone() };
        assert_eq!(verify(&tampered, &pk), Err("bad signature".into()));
        let mut sig = env.sig.clone().into_bytes();
        sig[10] = if sig[10] == b'0' { b'1' } else { b'0' };
        let flipped = Envelope { sig: String::from_utf8(sig).unwrap(), ..env };
        assert_eq!(verify(&flipped, &pk), Err("bad signature".into()));
        assert!(unhex::<32>(RELEASE_KEY).is_some());
    }

    #[test]
    fn signed_manifests_with_user_paths_are_refused() {
        let (sk, pk) = key();
        for path in ["memburn.cfg", "EFI/BOOT/memburn.cfg", "memburn-history.txt", "../x", "/EFI/a", "x.txt"] {
            let mut m = manifest();
            m.files[0].path = path.into();
            let env = sign(&sk, &serde_json::to_string(&m).unwrap());
            assert!(verify(&env, &pk).is_err(), "{path}");
        }
        assert!(writable("EFI/BOOT/BOOTX64.EFI") && writable("README.txt") && writable("windows/memburn.exe"));
        assert!(!writable("memburn-crash-2026-10-12-1102.txt") && !writable("EFI") && !writable("EFI//a"));
    }

    #[test]
    fn versions() {
        assert!(newer("0.10.0", "0.9.9") && newer("1.0.0", "0.99.0") && !newer("0.5.0", "0.5.0"));
        assert!(!newer("0.5", "0.4.0") && !newer("0.5.0.1", "0.4.0") && !newer("x", "0.1.0"));
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
