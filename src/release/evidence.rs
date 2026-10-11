use super::config::{relative, Command};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buf = [0u8; 1024 * 128];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub fn nonce() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}
pub fn workspace() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("deliver-release-{}", nonce()));
    fs::create_dir(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(path.canonicalize()?)
}
pub fn write(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
pub fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
/// Exact file set, regular files only; no symlink traversal, even at the root.
pub fn inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "artifact contains a symlink"
        );
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                visit(root, &entry?.path(), files)?;
            }
        } else {
            ensure!(meta.is_file(), "artifact contains a special file");
            let relative_path = path.strip_prefix(root)?;
            let name = relative_path
                .to_str()
                .context("artifact name is not UTF-8")?
                .to_owned();
            let name = if cfg!(windows) {
                name.replace('\\', "/")
            } else {
                name
            };
            relative(Path::new(&name))?;
            files.insert(name, hash_file(path)?);
        }
        Ok(())
    }
    ensure!(
        fs::symlink_metadata(root)?.is_dir(),
        "artifact root must be a directory"
    );
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    ensure!(!files.is_empty(), "artifact directory is empty");
    Ok(files)
}
pub fn verify(root: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    ensure!(
        inventory(root)? == *files,
        "artifact bytes or file set changed"
    );
    Ok(())
}
pub fn copy(root: &Path, output: &Path, files: &BTreeMap<String, String>) -> Result<()> {
    fs::create_dir_all(output)?;
    for name in files.keys() {
        relative(Path::new(name))?;
        let dest = output.join(name);
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::copy(root.join(name), dest)?;
    }
    verify(output, files)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub archive: String,
    pub image_id: String,
    /// Saved tag, for loading on a host whose Docker store uses a different ID kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_ref: Option<String>,
    /// Config digest from the saved archive. Defaults to image_id for classic Docker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_digest: Option<String>,
}
impl Image {
    pub fn config_digest(&self) -> &str {
        self.config_digest.as_deref().unwrap_or(&self.image_id)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub command: Command,
    pub platform: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub at: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    pub app: String,
    pub target: String,
    pub id: String,
    pub version: Option<String>,
    pub commit: String,
    pub clean: bool,
    pub config_sha256: String,
    pub cli_version: String,
    pub built_at: u64,
    pub build: Vec<Check>,
    pub required_checks: Vec<String>,
    pub files: BTreeMap<String, String>,
    pub images: BTreeMap<String, Image>,
    pub changelog: String,
    pub tickets: Vec<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckReceipt {
    pub manifest_sha256: String,
    pub commit: String,
    pub config_sha256: String,
    pub target: String,
    pub check: Check,
}
pub fn sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
pub fn image_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(sha256)
}
