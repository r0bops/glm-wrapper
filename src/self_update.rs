use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Version embedded at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const RELEASE_URL: &str = "https://api.github.com/repos/r0bops/glm-wrapper/releases/latest";

#[derive(Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<ReleaseAsset>,
}

/// The expected asset stem for this platform: glm-<os>-<arch>.
pub fn asset_stem() -> String {
    format!(
        "glm-{}-{}",
        crate::paths::os_name(),
        crate::paths::arch_name()
    )
}

/// Compare dotted versions; a > b => Greater. Ignores a leading v.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let num = |s: &str| s.trim_start_matches('v').to_string();
    let av: Vec<u64> = num(a).split('.').map(|p| p.parse().unwrap_or(0)).collect();
    let bv: Vec<u64> = num(b).split('.').map(|p| p.parse().unwrap_or(0)).collect();
    for i in 0..av.len().max(bv.len()) {
        match av
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&bv.get(i).copied().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

pub fn fetch_release(timeout: Duration) -> Result<Release> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let resp = agent
        .get(RELEASE_URL)
        .set("User-Agent", concat!("glm/", env!("CARGO_PKG_VERSION")))
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("fetching latest release from GitHub")?;
    let text = resp.into_string().context("reading release response")?;
    serde_json::from_str(&text).context("parsing release metadata")
}

/// Find the asset for this os/arch and the SHA256SUMS asset.
pub fn pick_assets<'a>(
    release: &'a Release,
    stem: &str,
) -> Result<(&'a ReleaseAsset, &'a ReleaseAsset)> {
    let target = format!("{stem}.tar.gz");
    let tar = release
        .assets
        .iter()
        .find(|a| a.name == target)
        .ok_or_else(|| anyhow::anyhow!("release has no {target} asset"))?;
    let sums = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .ok_or_else(|| anyhow::anyhow!("release has no SHA256SUMS asset"))?;
    Ok((tar, sums))
}

/// Parse "hash  filename" lines; filename may include a directory prefix.
pub fn parse_sha256sums(body: &str) -> Vec<(String, String)> {
    body.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let hash = it.next()?.to_string();
            let file = it.next()?.rsplit('/').next()?.to_string();
            Some((hash, file))
        })
        .collect()
}

pub fn expected_sha256(body: &str, target: &str) -> Option<String> {
    parse_sha256sums(body)
        .into_iter()
        .find(|(_, f)| f == target)
        .map(|(h, _)| h)
}

fn download(url: &str, timeout: Duration) -> Result<Vec<u8>> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let resp = agent
        .get(url)
        .set("User-Agent", concat!("glm/", env!("CARGO_PKG_VERSION")))
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .take(64 * 1024 * 1024)
        .read_to_end(&mut buf)
        .context("reading download")?;
    Ok(buf)
}

pub fn download_public(url: &str) -> Result<Vec<u8>> {
    download(url, Duration::from_secs(120))
}

/// Verify the sha256 of `data` against a hex digest.
pub fn verify_sha256(data: &[u8], want: &str) -> bool {
    hex_encode(&sha256(data)) == want.to_ascii_lowercase()
}

/// Replace `path` with `data` atomically: write a temp sibling, rename over.
pub fn install_bytes(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for {}", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.glm-update.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("glm"),
        std::process::id()
    ));
    std::fs::write(&tmp, data).context("writing update to temp file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755));
    }
    std::fs::rename(&tmp, path).context("replacing binary")?;
    Ok(())
}

/// Extract the `glm` binary from the release tarball (gzip + ustar).
pub fn extract_glm_binary(data: &[u8]) -> Option<Vec<u8>> {
    let raw = gunzip(data)?;
    let mut pos = 0usize;
    while pos + 512 <= raw.len() {
        let hdr = &raw[pos..pos + 512];
        if hdr.iter().all(|&b| b == 0) {
            return None; // end-of-archive marker
        }
        let name_end = hdr.iter().position(|&b| b == 0).unwrap_or(512);
        let name = String::from_utf8_lossy(&hdr[..name_end]).into_owned();
        let size = octal(&hdr[124..136])?;
        let data_start = pos + 512;
        let data_end = data_start.checked_add(size)?;
        let content = raw.get(data_start..data_end)?;
        if name == "glm" || name.ends_with("/glm") || name.ends_with("/glm.exe") {
            return Some(content.to_vec());
        }
        pos = data_end.checked_add(size.div_ceil(512) * 512)?;
        if pos + 512 > raw.len() {
            return None;
        }
    }
    None
}

fn octal(field: &[u8]) -> Option<usize> {
    let end = field
        .iter()
        .position(|&b| b == 0 || b == b' ')
        .unwrap_or(field.len());
    let s = std::str::from_utf8(&field[..end]).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    usize::from_str_radix(s, 8).ok()
}

/// RFC 1952 gzip decode via flate2 (pure-Rust backend).
fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

use std::io::Read;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            hex_encode(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex_encode(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn version_cmp_handles_leading_v() {
        assert_eq!(version_cmp("v0.2.0", "0.1.9"), std::cmp::Ordering::Greater);
        assert_eq!(version_cmp("0.1.0", "v0.1.0"), std::cmp::Ordering::Equal);
        assert_eq!(version_cmp("0.1.0", "0.1.1"), std::cmp::Ordering::Less);
        assert_eq!(version_cmp("0.10.0", "0.9.1"), std::cmp::Ordering::Greater);
    }

    #[test]
    fn sums_parsing_and_lookup() {
        let body = "abc123  glm-linux-x86_64.tar.gz\ndef456  dist/glm-darwin-aarch64.tar.gz\n";
        let sums = parse_sha256sums(body);
        assert_eq!(sums.len(), 2);
        assert_eq!(
            expected_sha256(body, "glm-linux-x86_64.tar.gz").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            expected_sha256(body, "glm-darwin-aarch64.tar.gz").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_sha256(body, "nope"), None);
    }

    #[test]
    fn verify_checksum_accepts_correct_hash() {
        let data = b"the quick brown fox";
        let hash = hex_encode(&sha256(data));
        assert!(verify_sha256(data, &hash));
        assert!(!verify_sha256(
            data,
            "0000000000000000000000000000000000000000000000000000000000000000"
        ));
    }

    #[test]
    fn asset_stem_matches_platform() {
        let s = asset_stem();
        assert!(s.starts_with("glm-"));
    }

    #[test]
    fn extracts_glm_from_real_tarball_fixture() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/glm-linux-x86_64.tar.gz"
        );
        let data = std::fs::read(path).unwrap();
        let bin = extract_glm_binary(&data).expect("fixture tarball must contain glm");
        assert_eq!(
            String::from_utf8_lossy(&bin),
            "fake-glm-binary-payload-42\n"
        );
    }

    #[test]
    fn sha256_of_tarball_matches_committed_sums() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/glm-linux-x86_64.tar.gz"
        );
        let data = std::fs::read(path).unwrap();
        let sums = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/SHA256SUMS"
        ))
        .unwrap();
        let expected = expected_sha256(&sums, "glm-linux-x86_64.tar.gz").unwrap();
        assert!(verify_sha256(&data, &expected));
    }
}
