//! The release notice: what bateri reads (on start and once a day) to learn that a newer helper
//! exists, and what to check the download against.
//!
//! It sits beside the archive in a release, with a `SHA256SUMS` file. bateri trusts neither the
//! notice nor the sums alone: the archive's digest must match, and the unpacked app's signature
//! must be ours (`SecStaticCodeCheckValidity` with our team identifier).

use serde::{Deserialize, Serialize};

/// The notice's own version; a bump means an older bateri must not read it.
pub const FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub format: u32,
    /// The helper's version.
    pub version: String,
    /// The Chromium it embeds.
    pub chromium: String,
    /// `message::PROTOCOL` of this helper; bateri only installs one it speaks.
    pub protocol: u32,
    /// The oldest helper bateri may run: a notice that names a version below it is a downgrade.
    pub minimum: String,
    pub assets: Vec<Asset>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Asset {
    /// `arm64` or `x86_64`.
    pub arch: String,
    /// The file's name in the release (no directories).
    pub name: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
}

impl Manifest {
    /// Parses and checks a notice: a known format, plain asset names, well-formed digests.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let manifest: Manifest = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if manifest.format != FORMAT {
            return Err(format!("notice format {} is not {FORMAT}", manifest.format));
        }
        for asset in &manifest.assets {
            let plain = !asset.name.is_empty()
                && !asset.name.contains(['/', '\\'])
                && !asset.name.starts_with('.');
            if !plain {
                return Err(format!(
                    "asset name {:?} is not a plain file name",
                    asset.name
                ));
            }
            let digest = asset.sha256.len() == 64
                && asset
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !digest {
                return Err(format!("asset {:?} has a malformed digest", asset.name));
            }
        }
        Ok(manifest)
    }

    pub fn asset(&self, arch: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.arch == arch)
    }
}
