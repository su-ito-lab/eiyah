// ==================================================
// @file src/lifecycle/mod.rs
// @brief Eiyah lifecycle command modules
// ==================================================

mod install;
mod uninstall;
mod update;

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const GITHUB_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_API_VERSION: &str = "2026-03-10";
const CONNECT_TIMEOUT_SECONDS: u64 = 5;
const GLOBAL_TIMEOUT_SECONDS: u64 = 30;
const REDIRECT_LIMIT: u32 = 10;
const SHA256_HEX_LENGTH: usize = 64;

pub(super) use install::run_install;
pub(super) use uninstall::run_uninstall;
pub(super) use update::run_update;

// 全GitHub requestへtimeout・redirect・User-Agent policyを適用する
fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(CONNECT_TIMEOUT_SECONDS)))
        .timeout_global(Some(Duration::from_secs(GLOBAL_TIMEOUT_SECONDS)))
        .max_redirects(REDIRECT_LIMIT)
        .https_only(true)
        .user_agent(format!("eiyah/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

// Release通信をHTTPS URLだけへ制限する
fn require_https_url(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("Release asset URL must use HTTPS: {url}");
    }
    Ok(())
}

/// candidate fileのSHA-256がRelease checksumと一致することを検証する
fn verify_checksum(path: &Path, expected: &[u8; 32]) -> Result<()> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("failed to open candidate {}", path.display()))?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    let calculated = hasher.finalize();
    if calculated.as_slice() != expected {
        bail!("downloaded Eiyah checksum does not match");
    }
    Ok(())
}

#[cfg(test)]
mod test_support {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::Result;

    use crate::config::{InstallMetadata, ResolvedPaths, save_install_metadata};

    // 並列test間でtemporary directory名が衝突しないための連番
    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    // lifecycle test専用directoryを所有するfixture
    pub(super) struct TestDirectory {
        // fixtureが所有するtemporary directory path
        pub(super) path: PathBuf,
    }

    impl TestDirectory {
        // process IDと連番からtest directoryを作成する
        pub(super) fn new() -> Result<Self> {
            let sequence = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "eiyah-install-state-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TestDirectory {
        // test終了時にfixture配下だけをcleanupする
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    // fixture配下に閉じたexpected pathを構成する
    pub(super) fn fixture_paths(root: &Path) -> Result<ResolvedPaths> {
        ResolvedPaths::from_install_metadata(InstallMetadata {
            config_home: root.join("config"),
            data_home: root.join("data"),
            state_home: root.join("state"),
            cache_home: root.join("cache"),
        })
    }

    // Installed判定に必要なbinary / entry / metadataを作成する
    pub(super) fn create_installed_fixture(
        paths: &ResolvedPaths,
        public_entry: &Path,
    ) -> Result<()> {
        let binary = paths.eiyah_prefix.join("bin/eiyah");
        fs::create_dir_all(binary.parent().unwrap())?;
        fs::write(&binary, b"binary")?;
        let mut permissions = fs::metadata(&binary)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions)?;
        fs::create_dir_all(public_entry.parent().unwrap())?;
        symlink(&binary, public_entry)?;
        save_install_metadata(paths)
    }
}
