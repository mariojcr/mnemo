// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! `mnemo update`, only when a person asks: a bare checksum would come from the same registry and
//! credential as the binary, so CI signs a manifest (checksum, commit, ref, build number) with a
//! key the registry never sees; only keys compiled in from `deploy/release-key.pub` are trusted.

use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

pub const DEFAULT_URL: &str = "https://github.com/mariojcr/mnemo/releases";
pub const ASSET: &str = "mnemo-linux-x86_64";
/// Signed by CI with `RELEASE_SIGNING_KEY`; the signature is base64 Ed25519.
pub const MANIFEST: &str = "release.txt";
pub const SIG: &str = "release.txt.sig";
/// The binary is a few tens of MB.
const MAX_BYTES: usize = 256 * 1024 * 1024;
/// Every key in the file is trusted. To rotate: append the new key and release (signed with the
/// old one), then sign with the new one, and drop the old key once no older binaries remain in use.
const RELEASE_KEYS: &str = include_str!("../deploy/release-key.pub");

/// Where the files of a build live. A package registry serves `{base}/{version}/{file}`; GitHub
/// Releases, a base ending in `/releases`, serves a tag at `{base}/download/{tag}/{file}` and the
/// newest release at `{base}/latest/download/{file}`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    Registry,
    Releases,
}

impl Layout {
    fn of(url: &reqwest::Url) -> Layout {
        if url.path().trim_end_matches('/').ends_with("/releases") {
            Layout::Releases
        } else {
            Layout::Registry
        }
    }

    /// What `mnemo update` installs when no version is given.
    fn default_version(self) -> &'static str {
        match self {
            Layout::Registry => "main",
            Layout::Releases => "latest",
        }
    }
}

/// `v1.2.3`, or a pre-release such as `v1.2.3-rc.1`: the tags CI releases.
fn release_tag(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else {
        return false;
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let ident = |s: &str, ok: fn(&u8) -> bool| !s.is_empty() && s.bytes().all(|b| ok(&b));
    core.split('.').count() == 3
        && core.split('.').all(|p| ident(p, u8::is_ascii_digit))
        && pre.is_none_or(|p| p.split('.').all(|s| ident(s, u8::is_ascii_alphanumeric)))
}

pub struct Trust<'a> {
    pub keys: &'a [VerifyingKey],
    pub seq: Option<u64>,
    pub allow_downgrade: bool,
}

/// First-parent commit count: it only grows on main and, unlike a date, the committer cannot
/// choose it.
fn own_seq() -> Option<u64> {
    option_env!("MNEMO_SEQ").and_then(|s| s.trim().parse().ok())
}

pub async fn run(args: &[String]) -> Result<String, String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("published binaries exist only for Linux x86_64".into());
    }
    let mut version = None;
    let mut allow_downgrade = false;
    for a in args {
        match a.as_str() {
            "--allow-downgrade" => allow_downgrade = true,
            o if o.starts_with('-') => return Err(format!("unknown option: {o}")),
            v => version = Some(v),
        }
    }
    let base = std::env::var("MNEMO_UPDATE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let version = version.unwrap_or_else(|| {
        reqwest::Url::parse(&base)
            .map_or(Layout::Registry, |u| Layout::of(&u))
            .default_version()
    });
    let target = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("cannot locate this binary: {e}"))?;
    let token = std::env::var("MNEMO_UPDATE_TOKEN").unwrap_or_default();
    let before = crate::version();
    let keys = public_keys(RELEASE_KEYS);
    let trust = Trust {
        keys: &keys,
        seq: own_seq(),
        allow_downgrade,
    };
    install(&base, version, &target, &token, &trust).await?;
    let after = std::process::Command::new(&target)
        .arg("version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    Ok(format!(
        "{}: mnemo {before} → {}",
        target.display(),
        after.trim_start_matches("mnemo ")
    ))
}

#[derive(Debug, PartialEq)]
struct Release {
    commit: String,
    git_ref: String,
    seq: u64,
    sum: String,
}

fn parse_release(text: &str) -> Option<Release> {
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ').map(str::trim))
    };
    let commit = field("commit")?.to_ascii_lowercase();
    let git_ref = field("ref")?.to_string();
    let seq = field("seq")?.parse().ok()?;
    let sum = expected_sum(text, ASSET)?;
    (commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()) && !git_ref.is_empty())
        .then_some(Release {
            commit,
            git_ref,
            seq,
            sum,
        })
}

/// A valid signature is not enough: the registry could serve another signed build (another
/// branch posing as main or as a release, another commit, an older build).
fn matches_request(rel: &Release, version: &str, trust: &Trust) -> Result<(), String> {
    if version == "latest"
        && !rel
            .git_ref
            .strip_prefix("refs/tags/")
            .is_some_and(release_tag)
    {
        return Err(format!(
            "latest serves a build of {}, not a release: not installing",
            rel.git_ref
        ));
    }
    if release_tag(version) && rel.git_ref != format!("refs/tags/{version}") {
        return Err(format!(
            "{version} serves a build of {}: not installing",
            rel.git_ref
        ));
    }
    if version == "main" && rel.git_ref != "refs/heads/main" {
        return Err(format!(
            "main serves a build of {}, not of main: not installing",
            rel.git_ref
        ));
    }
    if let Some(want) = version.strip_prefix("sha-") {
        if want.is_empty() || !rel.commit.starts_with(&want.to_ascii_lowercase()) {
            return Err(format!(
                "{version} serves the build of commit {}: not installing",
                &rel.commit[..7]
            ));
        }
    }
    if let Some(mine) = trust.seq {
        if rel.seq < mine && !trust.allow_downgrade {
            return Err(format!(
                "{version} ({}) is an older build than this binary: installing it is a downgrade; if that is what you want, `mnemo update {version} --allow-downgrade`",
                &rel.commit[..7]
            ));
        }
    }
    Ok(())
}

pub async fn install(
    base: &str,
    version: &str,
    target: &Path,
    token: &str,
    trust: &Trust<'_>,
) -> Result<(), String> {
    if trust.keys.is_empty() {
        return Err("this binary carries no signing keys: it cannot verify anything".into());
    }
    let url = reqwest::Url::parse(base.trim_end_matches('/'))
        .map_err(|e| format!("MNEMO_UPDATE_URL is not a URL: {e}"))?;
    let layout = Layout::of(&url);
    // Any other name would serve whatever is there with nothing to check it against.
    let sha_ok = |h: &str| (7..=40).contains(&h.len()) && h.chars().all(|c| c.is_ascii_hexdigit());
    let valid = release_tag(version)
        || match layout {
            Layout::Registry => {
                version == "main" || version.strip_prefix("sha-").is_some_and(sha_ok)
            }
            Layout::Releases => version == "latest",
        };
    if !valid {
        let expected = match layout {
            Layout::Registry => "main, sha-<commit> of a build, or a release such as v0.2.0",
            Layout::Releases => "latest, or a release such as v0.2.0",
        };
        return Err(format!("invalid version: '{version}' ({expected})"));
    }
    // A binary that runs on every turn never travels in cleartext, except on this machine.
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(format!("{url} is not https: no cleartext downloads"));
    }
    let root = url.as_str().trim_end_matches('/');
    let at = |tag: &str| match layout {
        Layout::Registry => format!("{root}/{tag}"),
        Layout::Releases => format!("{root}/download/{tag}"),
    };
    let base = match (layout, version) {
        (Layout::Releases, "latest") => format!("{root}/latest/download"),
        _ => at(version),
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        // Nor can a redirect downgrade it to http (e.g. a blob store behind the registry).
        .https_only(!loopback)
        .build()
        .map_err(|e| e.to_string())?;
    let manifest = fetch(&http, &format!("{base}/{MANIFEST}"), token, 64 * 1024).await?;
    let sig = fetch(&http, &format!("{base}/{SIG}"), token, 1024).await?;
    if !signed_by(trust.keys, &manifest, &sig) {
        return Err(format!(
            "{base}/{MANIFEST} is not signed by any trusted key: not installing"
        ));
    }
    let rel = parse_release(&String::from_utf8_lossy(&manifest))
        .ok_or_else(|| format!("{base}/{MANIFEST} does not have the expected format"))?;
    matches_request(&rel, version, trust)?;
    // From the release the signed manifest names, not from `latest` again: a release published in
    // between would serve another binary.
    let dir = match rel.git_ref.strip_prefix("refs/tags/") {
        Some(tag) if version == "latest" => at(tag),
        _ => base.clone(),
    };
    let bin = fetch(&http, &format!("{dir}/{ASSET}"), token, MAX_BYTES).await?;
    let got = hex(&Sha256::digest(&bin));
    if got != rel.sum {
        return Err(format!(
            "the checksum of {ASSET} does not match (expected {}, got {got}): not installing",
            rel.sum
        ));
    }
    if !bin.starts_with(b"\x7fELF") {
        return Err(format!("{ASSET} is not an ELF executable: not installing"));
    }
    replace(target, &bin)
}

async fn fetch(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut req = http.get(url);
    if !token.is_empty() {
        req = req.header("Authorization", format!("token {token}"));
    }
    let mut resp = req.send().await.map_err(|e| format!("{url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}: HTTP {}", resp.status().as_u16()));
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("{url}: {e}"))? {
        if out.len() + chunk.len() > limit {
            return Err(format!("{url}: more than {limit} bytes, rejected"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// SPKI `PUBLIC KEY` blocks as `openssl pkey -pubout` writes them; non-Ed25519 keys are skipped.
fn public_keys(pem: &str) -> Vec<VerifyingKey> {
    // DER header of an Ed25519 SPKI; the 32 key bytes follow.
    const SPKI_ED25519: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    pem.split("-----BEGIN PUBLIC KEY-----")
        .skip(1)
        .filter_map(|block| {
            let body: String = block
                .split("-----END PUBLIC KEY-----")
                .next()?
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            let der = base64::engine::general_purpose::STANDARD
                .decode(body)
                .ok()?;
            let raw: [u8; 32] = der.strip_prefix(&SPKI_ED25519)?.try_into().ok()?;
            VerifyingKey::from_bytes(&raw).ok()
        })
        .collect()
}

/// Strict: rejects the malleable signatures that lax Ed25519 verification accepts.
fn signed_by(keys: &[VerifyingKey], msg: &[u8], sig: &[u8]) -> bool {
    let text = String::from_utf8_lossy(sig);
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(text.trim()) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&raw) else {
        return false;
    };
    keys.iter().any(|k| k.verify_strict(msg, &sig).is_ok())
}

fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let (sum, file) = (parts.next()?, parts.next()?);
        (file.trim_start_matches('*') == name
            && sum.len() == 64
            && sum.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| sum.to_ascii_lowercase())
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Same-directory rename: atomic, so running processes keep the old binary, nobody sees a partial
/// one, and a failure leaves it intact.
fn replace(target: &Path, bin: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let dir = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    let tmp = dir.join(format!(".mnemo-update-{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(&tmp)
        .and_then(|mut f| {
            f.write_all(bin)?;
            f.sync_all()
        })
        .and_then(|_| std::fs::rename(&tmp, target));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not write {}: {e}", target.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, extract::Path as UrlPath, routing::get};
    use std::os::unix::fs::PermissionsExt;

    const COMMIT: &str = "c84357d0a1b2c3d4e5f60718293a4b5c6d7e8f90";

    fn signer() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32])
    }

    fn sign(sk: &ed25519_dalek::SigningKey, msg: &str) -> String {
        use ed25519_dalek::Signer;
        base64::engine::general_purpose::STANDARD.encode(sk.sign(msg.as_bytes()).to_bytes())
    }

    fn manifest(sum: &str, commit: &str, git_ref: &str, seq: u64) -> String {
        format!("commit {commit}\nref {git_ref}\nseq {seq}\n{sum}  {ASSET}\n")
    }

    fn keys() -> Vec<VerifyingKey> {
        vec![signer().verifying_key()]
    }

    fn trust(keys: &[VerifyingKey]) -> Trust<'_> {
        Trust {
            keys,
            seq: None,
            allow_downgrade: false,
        }
    }

    /// `sig: None` signs the manifest with the test key; an empty signature is not served.
    async fn registry(bin: Vec<u8>, manifest: String, sig: Option<String>) -> String {
        let sig = sig.unwrap_or_else(|| sign(&signer(), &manifest));
        let app = Router::new().route(
            "/pkg/{version}/{file}",
            get(move |UrlPath((_v, file)): UrlPath<(String, String)>| {
                let (bin, manifest, sig) = (bin.clone(), manifest.clone(), sig.clone());
                async move {
                    use axum::response::IntoResponse;
                    match file.as_str() {
                        f if f == ASSET => bin.into_response(),
                        f if f == MANIFEST => manifest.into_response(),
                        f if f == SIG && !sig.is_empty() => sig.into_response(),
                        _ => axum::http::StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}/pkg")
    }

    /// GitHub Releases: `latest` names the newest release and serves only its manifest and
    /// signature, so a binary installed through it can only have come from the tag.
    async fn releases(bin: Vec<u8>, latest: String, tag: &str, at_tag: String) -> String {
        use axum::response::{IntoResponse, Response};
        let serve = |manifest: String, with_bin: bool, bin: Vec<u8>| {
            let sig = sign(&signer(), &manifest);
            move |file: String| -> Response {
                match file.as_str() {
                    f if f == ASSET && with_bin => bin.clone().into_response(),
                    f if f == MANIFEST => manifest.clone().into_response(),
                    f if f == SIG => sig.clone().into_response(),
                    _ => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }
        };
        let newest = serve(latest, false, Vec::new());
        let tagged = serve(at_tag, true, bin);
        let tag = tag.to_string();
        let app = Router::new()
            .route(
                "/o/r/releases/latest/download/{file}",
                get(move |UrlPath(file): UrlPath<String>| {
                    let r = newest(file);
                    async move { r }
                }),
            )
            .route(
                "/o/r/releases/download/{tag}/{file}",
                get(move |UrlPath((t, file)): UrlPath<(String, String)>| {
                    let r = if t == tag {
                        tagged(file)
                    } else {
                        axum::http::StatusCode::NOT_FOUND.into_response()
                    };
                    async move { r }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}/o/r/releases")
    }

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mnemo-update-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn try_install(
        base: &str,
        version: &str,
        trust: &Trust<'_>,
    ) -> (Result<(), String>, Vec<u8>) {
        let dir = scratch();
        let target = dir.join("mnemo");
        std::fs::write(&target, b"old").unwrap();
        let r = install(base, version, &target, "", trust).await;
        let left = std::fs::read(&target).unwrap();
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            1,
            "no temporary file is left behind"
        );
        std::fs::remove_dir_all(dir).unwrap();
        (r, left)
    }

    #[tokio::test]
    async fn a_signed_build_whose_sum_matches_replaces_the_binary() {
        let new = b"\x7fELF new build".to_vec();
        let m = manifest(&hex(&Sha256::digest(&new)), COMMIT, "refs/heads/main", 100);
        let base = registry(new.clone(), m, None).await;
        let dir = scratch();
        let target = dir.join("mnemo");
        std::fs::write(&target, b"old").unwrap();
        install(&base, "main", &target, "", &trust(&keys()))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), new);
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
        std::fs::remove_dir_all(dir).unwrap();
        let (r, left) = try_install(&base, "sha-c84357d", &trust(&keys())).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(left, new);
    }

    #[tokio::test]
    async fn a_wrong_sum_leaves_the_old_binary_untouched() {
        let m = manifest(&"0".repeat(64), COMMIT, "refs/heads/main", 100);
        let base = registry(b"\x7fELF tampered".to_vec(), m, None).await;
        let (r, left) = try_install(&base, "main", &trust(&keys())).await;
        assert!(r.unwrap_err().contains("does not match"));
        assert_eq!(left, b"old");
    }

    #[tokio::test]
    async fn a_manifest_without_a_trusted_signature_is_not_believed() {
        let bin = b"\x7fELF build".to_vec();
        let m = manifest(&hex(&Sha256::digest(&bin)), COMMIT, "refs/heads/main", 100);
        let other = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]);
        for (case, sig) in [
            ("signed by another key", sign(&other, &m)),
            ("signs another manifest", sign(&signer(), "another")),
            ("signature not in base64", "not a signature".to_string()),
            ("no signature", String::new()),
        ] {
            let base = registry(bin.clone(), m.clone(), Some(sig)).await;
            let (r, left) = try_install(&base, "main", &trust(&keys())).await;
            let err = r.unwrap_err();
            assert!(
                err.contains("signed") || err.contains("HTTP 404"),
                "{case}: {err}"
            );
            assert_eq!(left, b"old", "{case}");
        }
        let base = registry(bin.clone(), m.clone(), None).await;
        let (r, left) = try_install(&base, "main", &trust(&[])).await;
        assert!(r.is_err());
        assert_eq!(left, b"old");
    }

    #[tokio::test]
    async fn another_signed_build_is_not_served_in_place_of_the_one_asked() {
        let bin = b"\x7fELF build".to_vec();
        let sum = hex(&Sha256::digest(&bin));
        let base = registry(
            bin.clone(),
            manifest(&sum, COMMIT, "refs/heads/feature", 100),
            None,
        )
        .await;
        let (r, left) = try_install(&base, "main", &trust(&keys())).await;
        assert!(r.unwrap_err().contains("not of main"));
        assert_eq!(left, b"old");
        let base = registry(
            bin.clone(),
            manifest(&sum, COMMIT, "refs/heads/main", 100),
            None,
        )
        .await;
        let (r, left) = try_install(&base, "sha-0123456", &trust(&keys())).await;
        assert!(r.unwrap_err().contains("c84357d"));
        assert_eq!(left, b"old");
        let older = Trust {
            keys: &keys(),
            seq: Some(200),
            allow_downgrade: false,
        };
        let (r, left) = try_install(&base, "main", &older).await;
        assert!(r.unwrap_err().contains("--allow-downgrade"));
        assert_eq!(left, b"old");
        let asked = Trust {
            keys: &keys(),
            seq: Some(200),
            allow_downgrade: true,
        };
        let (r, left) = try_install(&base, "main", &asked).await;
        assert!(r.is_ok(), "{r:?}");
        assert_eq!(left, bin);
    }

    #[tokio::test]
    async fn the_latest_release_is_installed_from_its_own_tag() {
        let new = b"\x7fELF v0.2.0".to_vec();
        let m = manifest(&hex(&Sha256::digest(&new)), COMMIT, "refs/tags/v0.2.0", 120);
        let base = releases(new.clone(), m.clone(), "v0.2.0", m).await;
        for version in ["latest", "v0.2.0"] {
            let (r, left) = try_install(&base, version, &trust(&keys())).await;
            assert!(r.is_ok(), "{version}: {r:?}");
            assert_eq!(left, new, "{version}");
        }
    }

    #[tokio::test]
    async fn a_releases_source_installs_only_the_release_asked_for() {
        let bin = b"\x7fELF build".to_vec();
        let sum = hex(&Sha256::digest(&bin));
        let branch = manifest(&sum, COMMIT, "refs/heads/main", 120);
        let base = releases(bin.clone(), branch.clone(), "v0.2.0", branch).await;
        let (r, left) = try_install(&base, "latest", &trust(&keys())).await;
        assert!(r.unwrap_err().contains("not a release"));
        assert_eq!(left, b"old");
        let older = manifest(&sum, COMMIT, "refs/tags/v0.1.0", 110);
        let base = releases(bin.clone(), older.clone(), "v0.2.0", older).await;
        let (r, left) = try_install(&base, "v0.2.0", &trust(&keys())).await;
        assert!(r.unwrap_err().contains("refs/tags/v0.1.0"));
        assert_eq!(left, b"old");
        let installed = Trust {
            keys: &keys(),
            seq: Some(120),
            allow_downgrade: false,
        };
        let (r, left) = try_install(&base, "latest", &installed).await;
        assert!(r.unwrap_err().contains("--allow-downgrade"));
        assert_eq!(left, b"old");
        for version in ["main", "sha-c84357d", "v0.2", "0.2.0"] {
            let (r, _) = try_install(&base, version, &trust(&keys())).await;
            assert!(r.unwrap_err().contains("invalid version"), "{version}");
        }
    }

    #[test]
    fn each_source_has_its_own_default_and_release_tags_are_strict() {
        let at = |u: &str| Layout::of(&reqwest::Url::parse(u).unwrap());
        assert_eq!(
            at("https://github.com/o/r/releases").default_version(),
            "latest"
        );
        assert_eq!(
            at("https://github.com/o/r/releases/").default_version(),
            "latest"
        );
        assert_eq!(at(DEFAULT_URL).default_version(), "latest");
        assert_eq!(
            at("https://example.com/api/packages/o/generic/mnemo").default_version(),
            "main"
        );
        for ok in ["v0.1.0", "v12.0.3", "v1.2.3-rc.1", "v1.2.3-beta"] {
            assert!(release_tag(ok), "{ok}");
        }
        for bad in [
            "0.1.0",
            "v1.2",
            "v1.2.3.4",
            "v1..3",
            "v1.2.x",
            "v1.2.3-",
            "v1.2.3-rc..1",
            "v1.2.3/..",
            "V1.2.3",
            "latest",
        ] {
            assert!(!release_tag(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn only_https_or_this_machine_and_plain_versions() {
        let target = std::env::temp_dir().join("mnemo-update-never-written");
        let err = install(
            "http://example.com/pkg",
            "main",
            &target,
            "",
            &trust(&keys()),
        )
        .await
        .unwrap_err();
        assert!(err.contains("https"), "{err}");
        for bad in [
            "",
            ".",
            "..",
            "../main",
            "main/x",
            "sha 1",
            "latest",
            "Main",
            "sha-",
            "sha-12",
            "sha-zzzzzzz",
        ] {
            let err = install("https://example.com/pkg", bad, &target, "", &trust(&keys()))
                .await
                .unwrap_err();
            assert!(err.contains("invalid version"), "{bad}: {err}");
        }
        assert!(!target.exists());
    }

    #[test]
    fn a_manifest_needs_commit_ref_date_and_sum() {
        let sum = "ab".repeat(32);
        let ok = manifest(&sum, COMMIT, "refs/heads/main", 812);
        assert_eq!(
            parse_release(&ok),
            Some(Release {
                commit: COMMIT.into(),
                git_ref: "refs/heads/main".into(),
                seq: 812,
                sum: sum.clone()
            })
        );
        assert_eq!(
            parse_release(&manifest(&sum, "c84357d", "refs/heads/main", 1)),
            None,
            "short commit"
        );
        assert_eq!(parse_release(&ok.replace("seq 812", "seq yesterday")), None);
        assert_eq!(parse_release(&ok.replace(&sum, "xyz")), None, "no checksum");
        assert_eq!(
            parse_release(&ok.replace("ref refs/heads/main\n", "")),
            None,
            "no branch"
        );
    }

    #[test]
    fn the_trusted_keys_are_read_from_the_pem_file() {
        let blocks = RELEASE_KEYS.matches("-----BEGIN PUBLIC KEY-----").count();
        assert!(blocks > 0, "deploy/release-key.pub carries the signing key");
        assert_eq!(
            public_keys(RELEASE_KEYS).len(),
            blocks,
            "every key in deploy/release-key.pub is a valid Ed25519 key: none is skipped"
        );
        let spki = |k: &VerifyingKey| {
            let mut der = vec![
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
            ];
            der.extend_from_slice(k.as_bytes());
            format!(
                "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                base64::engine::general_purpose::STANDARD.encode(der)
            )
        };
        let a = signer().verifying_key();
        let b = ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let pem = format!(
            "{}\n-----BEGIN PUBLIC KEY-----\nbm90IGEga2V5\n-----END PUBLIC KEY-----\n{}",
            spki(&a),
            spki(&b)
        );
        assert_eq!(public_keys(&pem), vec![a, b]);
    }

    /// A real `openssl pkeyutl -sign -rawin` vector, as CI signs (throwaway key, public half only).
    #[test]
    fn a_signature_made_by_openssl_like_the_ci_verifies() {
        let pem = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAoc8eYBtVnwv+4zOmVynlRUNRFQ08hkb74CnDW4bqeGo=\n-----END PUBLIC KEY-----\n";
        let msg = "e96269d87353c8790d25815fef0effbb1b7762061c4a46a0af69c0fd9207d211  mnemo-linux-x86_64\n";
        let sig = "NMvINl4d6sio+PUgCmbofZED0eeULrP8yK6Z6L4R8H6+ZusUC2+KGDsA/vGRVvAGqX/ChqPSPZA113bRmzJwAg==";
        let keys = public_keys(pem);
        assert!(signed_by(&keys, msg.as_bytes(), sig.as_bytes()));
        assert!(!signed_by(
            &keys,
            msg.replace("e962", "e963").as_bytes(),
            sig.as_bytes()
        ));
    }

    #[test]
    fn the_sum_is_read_from_sha256sum_output() {
        let sum = "ab".repeat(32);
        assert_eq!(
            expected_sum(&format!("{sum}  {ASSET}\n"), ASSET),
            Some(sum.clone())
        );
        assert_eq!(
            expected_sum(&format!("{}  *{ASSET}", sum.to_uppercase()), ASSET),
            Some(sum.clone())
        );
        assert_eq!(expected_sum(&format!("{sum}  other"), ASSET), None);
        assert_eq!(expected_sum(&format!("xyz  {ASSET}"), ASSET), None);
    }
}
