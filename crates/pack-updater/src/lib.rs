use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, USER_AGENT};
use rules_core::{unpack_pack_archive, verify_manifest, PackManifest, RulesCoreError};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

pub const DEFAULT_OWNER: &str = "CNI-KaeSoon";
pub const DEFAULT_REPO: &str = "cni-rules";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    CheckingLatestRelease,
    DownloadStarted {
        asset_name: String,
        total_bytes: Option<u64>,
    },
    Downloaded {
        bytes: u64,
        total_bytes: Option<u64>,
    },
    DownloadFinished {
        bytes: u64,
    },
    Unpacking,
    Verifying {
        path: String,
    },
    Installing,
    Installed {
        target_dir: PathBuf,
    },
}

pub trait ProgressCallback {
    fn on_progress(&self, event: ProgressEvent);
}

impl<F> ProgressCallback for F
where
    F: Fn(ProgressEvent),
{
    fn on_progress(&self, event: ProgressEvent) {
        self(event);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub manifest: PackManifest,
    pub target_dir: PathBuf,
    pub verified_files: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum PackUpdaterError {
    #[error("GitHub API request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("manifest JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("release has no pack-*.tar.zst asset")]
    MissingPackAsset,
    #[error("manifest schema_version must be 1, got {0}")]
    UnsupportedSchema(u32),
    #[error("manifest path is unsafe: {0}")]
    UnsafeManifestPath(String),
    #[error("manifest references missing file: {0}")]
    MissingManifestFile(String),
    #[error("pack contains unlisted file: {0}")]
    UnlistedPackFile(String),
    #[error("sha256 mismatch for {path}: expected {expected}, actual {actual}")]
    Sha256Mismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("target parent does not exist: {0}")]
    MissingTargetParent(PathBuf),
    #[error("archive entry is unsupported or unsafe: {0}")]
    UnsupportedArchiveEntry(String),
    #[error("pack validation failed: {0}")]
    RulesCore(RulesCoreError),
}

pub type Result<T> = std::result::Result<T, PackUpdaterError>;

impl From<RulesCoreError> for PackUpdaterError {
    fn from(err: RulesCoreError) -> Self {
        match err {
            RulesCoreError::Io(err) => Self::Io(err),
            RulesCoreError::Json(err) => Self::Json(err),
            RulesCoreError::UnsupportedSchema(version) => Self::UnsupportedSchema(version),
            RulesCoreError::UnsafePackPath(path) => Self::UnsafeManifestPath(path),
            RulesCoreError::MissingManifestEntry(path) => Self::MissingManifestFile(path),
            RulesCoreError::UnlistedPackFile(path) => Self::UnlistedPackFile(path),
            RulesCoreError::DigestMismatch {
                path,
                expected,
                actual,
            } => Self::Sha256Mismatch {
                path,
                expected,
                actual,
            },
            RulesCoreError::UnsupportedArchiveEntry(path) => Self::UnsupportedArchiveEntry(path),
            other => Self::RulesCore(other),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
}

pub fn latest_pack_asset(client: &Client, owner: &str, repo: &str) -> Result<ReleaseAsset> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let release: GitHubRelease = client
        .get(url)
        .header(USER_AGENT, "cni-rule-pack-updater/0.1")
        .header(ACCEPT, "application/vnd.github+json")
        .send()?
        .error_for_status()?
        .json()?;

    release
        .assets
        .into_iter()
        .find(|asset| asset.name.starts_with("pack-") && asset.name.ends_with(".tar.zst"))
        .map(|asset| ReleaseAsset {
            name: asset.name,
            download_url: asset.browser_download_url,
            size: asset.size,
        })
        .ok_or(PackUpdaterError::MissingPackAsset)
}

pub fn update_from_latest_release(
    target_dir: impl AsRef<Path>,
    progress: &impl ProgressCallback,
) -> Result<InstallReport> {
    update_from_github_release(DEFAULT_OWNER, DEFAULT_REPO, target_dir, progress)
}

pub fn update_from_github_release(
    owner: &str,
    repo: &str,
    target_dir: impl AsRef<Path>,
    progress: &impl ProgressCallback,
) -> Result<InstallReport> {
    progress.on_progress(ProgressEvent::CheckingLatestRelease);
    let client = Client::new();
    let asset = latest_pack_asset(&client, owner, repo)?;
    let archive = download_asset(&client, &asset, progress)?;
    install_pack_archive(&archive.path, target_dir, progress)
}

pub fn install_pack_archive(
    archive_path: impl AsRef<Path>,
    target_dir: impl AsRef<Path>,
    progress: &impl ProgressCallback,
) -> Result<InstallReport> {
    let target_dir = target_dir.as_ref();
    let parent = target_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| PackUpdaterError::MissingTargetParent(target_dir.to_path_buf()))?;
    if !parent.exists() {
        return Err(PackUpdaterError::MissingTargetParent(parent));
    }

    let temp = tempfile::Builder::new()
        .prefix("cni-pack-install-")
        .tempdir_in(&parent)?;
    let unpacked_dir = temp.path().join("unpacked");
    fs::create_dir(&unpacked_dir)?;

    progress.on_progress(ProgressEvent::Unpacking);
    unpack_pack_archive(archive_path.as_ref(), &unpacked_dir)?;
    let report = verify_unpacked_pack(&unpacked_dir, progress)?;

    progress.on_progress(ProgressEvent::Installing);
    replace_dir_with_rename(&unpacked_dir, target_dir)?;
    progress.on_progress(ProgressEvent::Installed {
        target_dir: target_dir.to_path_buf(),
    });

    Ok(InstallReport {
        manifest: report.manifest,
        target_dir: target_dir.to_path_buf(),
        verified_files: report.verified_files,
    })
}

struct DownloadedArchive {
    _temp: TempDir,
    path: PathBuf,
}

fn download_asset(
    client: &Client,
    asset: &ReleaseAsset,
    progress: &impl ProgressCallback,
) -> Result<DownloadedArchive> {
    let temp = tempfile::Builder::new()
        .prefix("cni-pack-download-")
        .tempdir()?;
    let archive_path = temp.path().join(&asset.name);
    let mut response = client
        .get(&asset.download_url)
        .header(USER_AGENT, "cni-rule-pack-updater/0.1")
        .send()?
        .error_for_status()?;
    let total_bytes = response.content_length().or(asset.size);
    progress.on_progress(ProgressEvent::DownloadStarted {
        asset_name: asset.name.clone(),
        total_bytes,
    });

    let mut file = File::create(&archive_path)?;
    let mut buf = [0_u8; 64 * 1024];
    let mut downloaded = 0_u64;
    loop {
        let read = response.read(&mut buf)?;
        if read == 0 {
            break;
        }
        file.write_all(&buf[..read])?;
        downloaded += read as u64;
        progress.on_progress(ProgressEvent::Downloaded {
            bytes: downloaded,
            total_bytes,
        });
    }
    file.flush()?;
    progress.on_progress(ProgressEvent::DownloadFinished { bytes: downloaded });
    Ok(DownloadedArchive {
        _temp: temp,
        path: archive_path,
    })
}

struct VerificationReport {
    manifest: PackManifest,
    verified_files: usize,
}

fn verify_unpacked_pack(
    unpacked_dir: &Path,
    progress: &impl ProgressCallback,
) -> Result<VerificationReport> {
    let manifest_path = unpacked_dir.join("manifest.json");
    let mut manifest: PackManifest = serde_json::from_reader(File::open(&manifest_path)?)?;
    manifest.normalize_hashes();
    if manifest.schema_version != 1 {
        return Err(PackUpdaterError::UnsupportedSchema(manifest.schema_version));
    }
    verify_manifest(unpacked_dir, &manifest)?;

    for (path, expected) in &manifest.files {
        progress.on_progress(ProgressEvent::Verifying { path: path.clone() });
        let relative_path = safe_relative_path(path)?;
        let file_path = unpacked_dir.join(&relative_path);
        if !file_path.is_file() {
            return Err(PackUpdaterError::MissingManifestFile(path.clone()));
        }
        let actual = file_sha256_hex(&file_path)?;
        if actual != *expected {
            return Err(PackUpdaterError::Sha256Mismatch {
                path: path.clone(),
                expected: expected.clone(),
                actual,
            });
        }
    }

    Ok(VerificationReport {
        verified_files: manifest.files.len(),
        manifest,
    })
}

#[cfg(test)]
fn normalize_manifest_paths(manifest: &PackManifest) -> Result<std::collections::BTreeSet<String>> {
    manifest
        .files
        .keys()
        .map(|path| safe_relative_path(path).map(|p| path_to_manifest_key(&p)))
        .collect()
}

fn safe_relative_path(path: &str) -> Result<PathBuf> {
    let raw = Path::new(path);
    if raw.is_absolute() || path.is_empty() {
        return Err(PackUpdaterError::UnsafeManifestPath(path.to_string()));
    }

    let mut normalized = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            _ => return Err(PackUpdaterError::UnsafeManifestPath(path.to_string())),
        }
    }

    if normalized.as_os_str().is_empty() {
        return Err(PackUpdaterError::UnsafeManifestPath(path.to_string()));
    }
    Ok(normalized)
}

#[cfg(test)]
fn path_to_manifest_key(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn file_sha256_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn replace_dir_with_rename(staged_dir: &Path, target_dir: &Path) -> Result<()> {
    let backup_dir = target_dir.with_extension(format!("old-{}", timestamp_millis()));
    if target_dir.exists() {
        fs::rename(target_dir, &backup_dir)?;
    }

    match fs::rename(staged_dir, target_dir) {
        Ok(()) => {
            if backup_dir.exists() {
                fs::remove_dir_all(backup_dir)?;
            }
            Ok(())
        }
        Err(err) => {
            if backup_dir.exists() {
                let _ = fs::rename(&backup_dir, target_dir);
            }
            Err(PackUpdaterError::Io(err))
        }
    }
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Cursor;
    use tar::{Builder, Header};
    use tempfile::tempdir;
    use zstd::stream::write::Encoder as ZstdEncoder;

    #[test]
    fn verifies_and_replaces_target_from_fixture_pack() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("pack-cni-2026-02-27.tar.zst");
        write_fixture_pack(
            &archive,
            &[
                ("graph/nodes.jsonl", br#"{"id":"a","kind":"Article"}"#),
                ("articles/test.md", b"body"),
                ("quality/report.json", br#"{"broken_edges":0}"#),
            ],
        );

        let target = temp.path().join("current-pack");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("stale.txt"), "old").unwrap();

        let events = std::sync::Mutex::new(Vec::new());
        let report = install_pack_archive(&archive, &target, &|event| {
            events.lock().unwrap().push(event);
        })
        .unwrap();

        assert_eq!(report.manifest.schema_version, 1);
        assert_eq!(report.verified_files, 3);
        assert!(target.join("articles/test.md").is_file());
        assert!(!target.join("stale.txt").exists());
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, ProgressEvent::Installed { .. })));
    }

    #[test]
    fn rejects_sha256_mismatch() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("pack-cni-2026-02-27.tar.zst");
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: BTreeMap::from([(
                "articles/test.md".to_string(),
                "0000000000000000000000000000000000000000000000000000000000000000".to_string(),
            )]),
        };
        write_pack_with_manifest(&archive, &[("articles/test.md", b"body")], &manifest);

        let target = temp.path().join("current-pack");
        let err = install_pack_archive(&archive, &target, &|_| {}).unwrap_err();
        assert!(matches!(err, PackUpdaterError::Sha256Mismatch { .. }));
        assert!(!target.exists());
    }

    #[test]
    fn rejects_unlisted_files() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("pack-cni-2026-02-27.tar.zst");
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: BTreeMap::from([("articles/test.md".to_string(), sha256_hex(b"body"))]),
        };
        write_pack_with_manifest(
            &archive,
            &[("articles/test.md", b"body"), ("extra.txt", b"nope")],
            &manifest,
        );

        let err =
            install_pack_archive(&archive, temp.path().join("current-pack"), &|_| {}).unwrap_err();
        assert!(matches!(err, PackUpdaterError::UnlistedPackFile(path) if path == "extra.txt"));
    }

    #[test]
    fn rejects_unsafe_manifest_path() {
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: BTreeMap::from([("../escape.txt".to_string(), sha256_hex(b"escape"))]),
        };

        let err = normalize_manifest_paths(&manifest).unwrap_err();
        assert!(matches!(err, PackUpdaterError::UnsafeManifestPath(_)));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("pack-cni-2026-02-27.tar.zst");
        let manifest = PackManifest {
            schema_version: 2,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: BTreeMap::from([("articles/test.md".to_string(), sha256_hex(b"body"))]),
        };
        write_pack_with_manifest(&archive, &[("articles/test.md", b"body")], &manifest);

        let err =
            install_pack_archive(&archive, temp.path().join("current-pack"), &|_| {}).unwrap_err();
        assert!(matches!(err, PackUpdaterError::UnsupportedSchema(2)));
    }

    #[test]
    fn rejects_manifest_referencing_file_missing_from_archive() {
        let temp = tempdir().unwrap();
        let archive = temp.path().join("pack-cni-2026-02-27.tar.zst");
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: BTreeMap::from([
                ("articles/test.md".to_string(), sha256_hex(b"body")),
                (
                    "articles/never-shipped.md".to_string(),
                    sha256_hex(b"phantom"),
                ),
            ]),
        };
        // Archive only contains the manifest + one of the two listed files.
        write_pack_with_manifest(&archive, &[("articles/test.md", b"body")], &manifest);

        let err =
            install_pack_archive(&archive, temp.path().join("current-pack"), &|_| {}).unwrap_err();
        assert!(matches!(
            err,
            PackUpdaterError::MissingManifestFile(path) if path == "articles/never-shipped.md"
        ));
    }

    #[test]
    fn rejects_path_traversal_entry_crafted_in_raw_archive_bytes() {
        // A well-behaved writer (tar::Builder::append_data) refuses to encode a
        // ".." path component at all, so this simulates a maliciously crafted
        // (or corrupted) release asset that a real GitHub Release download could
        // contain: we poke the entry name bytes directly, bypassing tar-rs's own
        // write-side validation, to prove the *unpack* side independently
        // rejects zip-slip / tar-slip style escapes rather than relying only on
        // manifest-path validation.
        let temp = tempdir().unwrap();
        let archive = temp.path().join("pack-cni-2026-02-27.tar.zst");

        let file = File::create(&archive).unwrap();
        let encoder = ZstdEncoder::new(file, 0).unwrap();
        let mut tar = Builder::new(encoder);

        // Legit manifest entry.
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: BTreeMap::new(),
        };
        append_bytes(
            &mut tar,
            "manifest.json",
            &serde_json::to_vec(&manifest).unwrap(),
        );

        // Malicious entry: name bytes set directly to "../evil.txt" without
        // going through Header::set_path (which would reject `..`).
        let contents = b"pwned";
        let mut header = Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        {
            let name_field = &mut header.as_old_mut().name;
            name_field.fill(0);
            let raw = b"../evil.txt";
            name_field[..raw.len()].copy_from_slice(raw);
        }
        header.set_cksum();
        tar.append(&header, Cursor::new(&contents[..])).unwrap();

        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();

        let target = temp.path().join("current-pack");
        let err = install_pack_archive(&archive, &target, &|_| {}).unwrap_err();
        assert!(matches!(err, PackUpdaterError::UnsupportedArchiveEntry(_)));
        assert!(!target.exists());
        // The traversal target itself must never be written.
        assert!(!temp.path().join("evil.txt").exists());
    }

    #[test]
    #[ignore = "hits the public GitHub Releases API"]
    fn finds_latest_public_pack_asset() {
        let client = Client::new();
        let asset = latest_pack_asset(&client, DEFAULT_OWNER, DEFAULT_REPO).unwrap();
        assert!(asset.name.starts_with("pack-"));
        assert!(asset.name.ends_with(".tar.zst"));
    }

    fn write_fixture_pack(path: &Path, files: &[(&str, &[u8])]) {
        let manifest_files = files
            .iter()
            .map(|(path, contents)| ((*path).to_string(), sha256_hex(contents)))
            .collect();
        let manifest = PackManifest {
            schema_version: 1,
            institution: "cni".to_string(),
            effective_date: "2026-02-27".to_string(),
            source_commit: "abc123".to_string(),
            created_at: "2026-07-02T00:00:00Z".to_string(),
            files: manifest_files,
        };
        write_pack_with_manifest(path, files, &manifest);
    }

    fn write_pack_with_manifest(path: &Path, files: &[(&str, &[u8])], manifest: &PackManifest) {
        let file = File::create(path).unwrap();
        let encoder = ZstdEncoder::new(file, 0).unwrap();
        let mut tar = Builder::new(encoder);
        append_bytes(
            &mut tar,
            "manifest.json",
            &serde_json::to_vec(manifest).unwrap(),
        );
        for (path, contents) in files {
            append_bytes(&mut tar, path, contents);
        }
        let encoder = tar.into_inner().unwrap();
        encoder.finish().unwrap();
    }

    fn append_bytes(builder: &mut Builder<ZstdEncoder<File>>, path: &str, contents: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(contents))
            .unwrap();
    }

    fn sha256_hex(contents: &[u8]) -> String {
        hex::encode(Sha256::digest(contents))
    }
}
