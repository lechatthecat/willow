//! Materialize verified native inputs beside the final executable. Temporary
//! directories are owned by this operation; existing foreign content is never
//! replaced or recursively deleted.
use super::*;
use std::io::Write;
use willow_abi::native::{Distribution, POLICY_FILE, SCHEMA};

#[derive(Debug, Clone)]
pub struct SharedArtifact {
    pub source: PathBuf,
    pub library: willow_abi::native::Library,
}
pub struct Prepared {
    target: PathBuf,
    staging: Option<PathBuf>,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(path) = &self.staging {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn owned(path: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "native_distribution_escape"
    );
    let policy_path = path.join(POLICY_FILE);
    let meta = std::fs::symlink_metadata(&policy_path)?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.len() <= willow_abi::native::MAX_POLICY_BYTES,
        "native_distribution_foreign_content"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(policy_path)?
        .take(willow_abi::native::MAX_POLICY_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= willow_abi::native::MAX_POLICY_BYTES,
        "native_policy_too_large"
    );
    let policy: Distribution = serde_json::from_slice(&bytes)?;
    policy.validate().map_err(anyhow::Error::msg)?;
    let names: std::collections::HashSet<_> = policy
        .libraries
        .iter()
        .map(|l| l.policy.distribution_name.as_str())
        .chain(Some(POLICY_FILE))
        .collect();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_file()
                && names.contains(entry.file_name().to_str().unwrap_or("")),
            "native_distribution_foreign_content"
        );
    }
    Ok(())
}
pub fn prepare(executable: &Path, artifacts: &[SharedArtifact]) -> Result<Prepared> {
    let target = willow_abi::native::directory(executable).map_err(anyhow::Error::msg)?;
    if target.try_exists()? {
        owned(&target)?;
    }
    let mut prepared = Prepared {
        target,
        staging: None,
    };
    if artifacts.is_empty() {
        return Ok(prepared);
    }
    let distribution = Distribution {
        schema: SCHEMA,
        libraries: artifacts.iter().map(|a| a.library.clone()).collect(),
    };
    distribution.validate().map_err(anyhow::Error::msg)?;
    let policy = serde_json::to_vec(&distribution)?;
    ensure!(
        policy.len() as u64 <= willow_abi::native::MAX_POLICY_BYTES,
        "native_policy_too_large"
    );
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let staging = prepared.target.with_extension(format!(
        "native-tmp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir(&staging)?;
    prepared.staging = Some(staging.clone());
    for artifact in artifacts {
        let path = staging.join(&artifact.library.policy.distribution_name);
        let mut source = std::fs::File::open(&artifact.source)?;
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let mut h = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = source.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            output.write_all(&buffer[..n])?;
            h.update(&buffer[..n]);
        }
        ensure!(
            format!("{:x}", crate::hex::Hex(&h.finalize())) == artifact.library.digest,
            "native_source_changed_before_distribution"
        );
        output.sync_all()?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(staging.join(POLICY_FILE))?;
    file.write_all(&policy)?;
    file.sync_all()?;
    Ok(prepared)
}
impl Prepared {
    pub fn publish(mut self) -> Result<()> {
        // A renamed previous directory permits rollback on publication failure.
        let backup = if self.target.try_exists()? {
            owned(&self.target)?;
            let backup = self
                .target
                .with_extension(format!("native-old-{}", std::process::id()));
            ensure!(!backup.try_exists()?, "native_distribution_backup_exists");
            std::fs::rename(&self.target, &backup)?;
            Some(backup)
        } else {
            None
        };
        if let Some(staging) = &self.staging {
            if let Err(error) = std::fs::rename(staging, &self.target) {
                if let Some(backup) = &backup {
                    std::fs::rename(backup, &self.target)?;
                }
                return Err(error.into());
            }
            self.staging = None;
        }
        if let Some(backup) = backup {
            std::fs::remove_dir_all(backup)?;
        }
        Ok(())
    }
}
