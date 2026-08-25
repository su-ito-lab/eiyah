// ==================================================
// @file src/install.rs
// @brief Installation state detection and binary update
// ==================================================

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Error, Result, bail};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{
    ResolvedPaths, discover_install_metadata, load_install_metadata, runtime_home,
};
use crate::transaction::LockGuard;

// Public Releaseを取得するGitHub API endpoint
const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/su-ito-lab/eiyah/releases/latest";
// GitHub APIで要求するmedia type
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
// GitHub API requestへ固定するversion
const GITHUB_API_VERSION: &str = "2026-03-10";
// Public Releaseに必須のLinux binary asset名
const BINARY_ASSET_NAME: &str = "eiyah-x86_64-unknown-linux-gnu";
// Public Releaseに必須のchecksum asset名
const CHECKSUM_ASSET_NAME: &str = "eiyah-x86_64-unknown-linux-gnu.sha256";
// atomic replacement前のcandidate file名
const CANDIDATE_FILE_NAME: &str = ".eiyah.new";
// download中のcandidate permission
const CANDIDATE_DOWNLOAD_MODE: u32 = 0o600;
// 検証・実行可能なcandidate permission
const CANDIDATE_EXECUTABLE_MODE: u32 = 0o755;
// GitHub接続確立までの上限秒数
const CONNECT_TIMEOUT_SECONDS: u64 = 5;
// request全体の上限秒数
const GLOBAL_TIMEOUT_SECONDS: u64 = 30;
// GitHubおよびasset requestのredirect上限
const REDIRECT_LIMIT: u32 = 10;
// SHA-256をlowercase hexで表した文字数
const SHA256_HEX_LENGTH: usize = 64;

/// 更新に使用するstable Public Releaseの情報
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseInfo {
    /// Release tagから復元した更新version
    pub version: Version,
    /// Linux binary assetのdownload URL
    pub binary_url: String,
    /// checksum assetのdownload URL
    pub checksum_url: String,
}

// GitHub latest Release responseで使用するfield
#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    // Release tag
    tag_name: String,
    // draft Releaseかを示すflag
    draft: bool,
    // prereleaseかを示すflag
    prerelease: bool,
    // Releaseに添付されたasset
    assets: Vec<ReleaseAsset>,
}

// required asset選択に使用するGitHub asset field
#[derive(Clone, Debug, Deserialize)]
struct ReleaseAsset {
    // assetのexact name
    name: String,
    // GitHubが返すasset download URL
    browser_download_url: String,
}

/// Public Eiyahのinstall状態
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallState {
    /// managed artifactがすべて存在しない状態
    NotInstalled,
    /// 必須artifactとmetadataがすべて整合する状態
    Installed,
    /// artifactの不足または構造不整合がある状態
    Partial,
}

/// expected pathとpublic entryからinstall状態を判定する
pub fn detect_install_state(paths: &ResolvedPaths, public_entry: &Path) -> Result<InstallState> {
    let binary = paths.eiyah_prefix.join("bin/eiyah");
    let metadata_path = paths.eiyah_prefix.join("install.toml");
    let public_entry_exists = path_exists(public_entry)?;
    let binary_exists = path_exists(&binary)?;
    let metadata_exists = path_exists(&metadata_path)?;

    if !public_entry_exists && !binary_exists && !metadata_exists {
        return Ok(InstallState::NotInstalled);
    }
    if !public_entry_exists || !binary_exists || !metadata_exists {
        return Ok(InstallState::Partial);
    }
    match fs::symlink_metadata(&metadata_path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => return Ok(InstallState::Partial),
        Err(error) if is_structural_error(&error) => return Ok(InstallState::Partial),
        Err(error) => return Err(error.into()),
    }
    match is_executable(&binary) {
        Ok(true) => {}
        Ok(false) => return Ok(InstallState::Partial),
        Err(error) if is_structural_failure(&error) => return Ok(InstallState::Partial),
        Err(error) => return Err(error),
    }

    let expected_target = &binary;
    let actual_target = match fs::read_link(public_entry) {
        Ok(target) => target,
        Err(error) if is_structural_error(&error) => return Ok(InstallState::Partial),
        Err(error) => return Err(error.into()),
    };
    if !actual_target.is_absolute() || actual_target != *expected_target {
        return Ok(InstallState::Partial);
    }

    let discovered_metadata = match discover_install_metadata(public_entry) {
        Ok(path) => path,
        Err(error) if is_structural_failure(&error) => return Ok(InstallState::Partial),
        Err(error) => return Err(error),
    };
    if discovered_metadata != metadata_path {
        return Ok(InstallState::Partial);
    }

    let metadata = match load_install_metadata(&metadata_path) {
        Ok(metadata) => metadata,
        Err(error) if is_structural_failure(&error) => return Ok(InstallState::Partial),
        Err(error) => return Err(error),
    };
    let metadata_paths = match ResolvedPaths::from_install_metadata(metadata) {
        Ok(paths) => paths,
        Err(_) => return Ok(InstallState::Partial),
    };
    if metadata_paths.eiyah_prefix != paths.eiyah_prefix {
        return Ok(InstallState::Partial);
    }

    Ok(InstallState::Installed)
}

// GitHubのlatest endpointからPublic Release responseを取得する
fn fetch_latest_release() -> Result<ReleaseResponse> {
    let agent = http_agent();
    let mut response = agent
        .get(LATEST_RELEASE_URL)
        .header("Accept", GITHUB_ACCEPT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .call()
        .context("failed to fetch latest Public Release")?;
    parse_release_response(response.body_mut())
}

// GitHub response bodyから更新判定に必要なfieldだけをdecodeする
fn parse_release_response(body: &mut ureq::Body) -> Result<ReleaseResponse> {
    body.read_json()
        .context("failed to parse latest Public Release response")
}

/// `v<SEMVER>` 形式のRelease tagをversionへ変換する
pub fn parse_release_version(tag: &str) -> Result<Version> {
    let version = tag
        .strip_prefix('v')
        .ok_or_else(|| anyhow::anyhow!("release tag must start with v: {tag}"))?;
    if version.is_empty() {
        bail!("release tag version is empty");
    }
    Version::parse(version).with_context(|| format!("invalid release version: {tag}"))
}

/// required binaryとchecksum assetをexact nameで選択する
fn select_release_assets(assets: &[ReleaseAsset]) -> Result<(String, String)> {
    let binary_url = select_release_asset(assets, BINARY_ASSET_NAME)?;
    let checksum_url = select_release_asset(assets, CHECKSUM_ASSET_NAME)?;
    Ok((binary_url, checksum_url))
}

/// HTTPS assetをsecure candidate fileへdownloadして同期する
pub fn download_to_file(url: &str, path: &Path) -> Result<()> {
    require_https_url(url)?;
    let agent = http_agent();
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(CANDIDATE_DOWNLOAD_MODE)
        .open(path)
        .with_context(|| format!("failed to create candidate {}", path.display()))?;
    io::copy(&mut response.body_mut().as_reader(), &mut file)
        .with_context(|| format!("failed to write candidate {}", path.display()))?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// HTTPS assetをchecksum textとしてdownloadする
pub fn download_text(url: &str) -> Result<String> {
    require_https_url(url)?;
    let agent = http_agent();
    agent
        .get(url)
        .call()
        .with_context(|| format!("failed to download {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read {url}"))
}

/// checksum assetのexact one-line formatを検証してdigestを返す
pub fn parse_checksum(text: &str) -> Result<[u8; 32]> {
    let expected_suffix = format!("  {BINARY_ASSET_NAME}\n");
    if text.len() != SHA256_HEX_LENGTH + expected_suffix.len() || !text.ends_with(&expected_suffix)
    {
        bail!("invalid checksum file format");
    }

    let hexadecimal = &text[..SHA256_HEX_LENGTH];
    if !hexadecimal
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("checksum must be lowercase SHA-256 hexadecimal");
    }

    let mut checksum = [0_u8; 32];
    for (index, byte) in checksum.iter_mut().enumerate() {
        let offset = index * 2;
        *byte = u8::from_str_radix(&hexadecimal[offset..offset + 2], 16)
            .context("failed to parse checksum")?;
    }
    Ok(checksum)
}

/// candidate fileのSHA-256がRelease checksumと一致することを検証する
pub fn verify_checksum(path: &Path, expected: &[u8; 32]) -> Result<()> {
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

/// Eiyah binaryの`--version` outputがexpected versionと一致することを検証する
pub fn validate_eiyah_binary(path: &Path, expected_version: &Version) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to execute {}", path.display()))?;
    if !output.status.success() {
        bail!("Eiyah version validation failed: {}", path.display());
    }
    let stdout =
        std::str::from_utf8(&output.stdout).context("Eiyah version output is not valid UTF-8")?;
    let expected = format!("eiyah {expected_version}");
    if stdout.trim() != expected {
        bail!(
            "unexpected Eiyah version output from {}: {}",
            path.display(),
            stdout.trim()
        );
    }
    Ok(())
}

/// installed metadataからpathを復元してexclusive lock内で更新する
pub fn run_update() -> Result<()> {
    let home = runtime_home()?;
    let metadata_path = discover_install_metadata(&home.join(".local/bin/eiyah"))?;
    let paths = ResolvedPaths::from_install_metadata(load_install_metadata(&metadata_path)?)?;
    let _lock = LockGuard::acquire(&paths.state_home)?;
    update_locked(&paths)
}

/// callerが保持するoperation lock内でEiyah binaryを更新する
pub fn update_locked(paths: &ResolvedPaths) -> Result<()> {
    update_locked_with(paths, fetch_latest_release, download_to_file, download_text)
}

// network dependencyを差し替え可能にして更新transactionを実行する
fn update_locked_with(
    paths: &ResolvedPaths,
    mut fetch_release: impl FnMut() -> Result<ReleaseResponse>,
    mut download_binary: impl FnMut(&str, &Path) -> Result<()>,
    mut download_checksum: impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let current_version =
        Version::parse(env!("CARGO_PKG_VERSION")).context("current package version is invalid")?;
    let response = fetch_release()?;
    let remote_version = validate_release_version(&response)?;
    if remote_version <= current_version {
        return Ok(());
    }
    let release = release_info(response, remote_version)?;

    let binary_directory = paths.eiyah_prefix.join("bin");
    let installed = binary_directory.join("eiyah");
    validate_installed_target(&installed)?;
    let candidate = binary_directory.join(CANDIDATE_FILE_NAME);
    prepare_candidate_path(&candidate)?;

    let prepared = (|| -> Result<()> {
        download_binary(&release.binary_url, &candidate)?;
        let checksum = parse_checksum(&download_checksum(&release.checksum_url)?)?;
        verify_checksum(&candidate, &checksum)?;

        let mut permissions = fs::symlink_metadata(&candidate)?.permissions();
        permissions.set_mode(CANDIDATE_EXECUTABLE_MODE);
        fs::set_permissions(&candidate, permissions)?;
        fs::File::open(&candidate)?.sync_all()?;
        validate_eiyah_binary(&candidate, &release.version)?;
        Ok(())
    })();
    if let Err(error) = prepared {
        cleanup_candidate(&candidate);
        return Err(error);
    }

    if let Err(error) = fs::rename(&candidate, &installed) {
        cleanup_candidate(&candidate);
        return Err(error.into());
    }
    validate_eiyah_binary(&installed, &release.version)
}

// stable flagとrequired assetから更新情報を組み立てる
fn validate_release_version(release: &ReleaseResponse) -> Result<Version> {
    if release.draft {
        bail!("latest Public Release is a draft");
    }
    if release.prerelease {
        bail!("latest Public Release is a prerelease");
    }
    parse_release_version(&release.tag_name)
}

// newer Releaseのrequired assetから更新情報を組み立てる
fn release_info(release: ReleaseResponse, version: Version) -> Result<ReleaseInfo> {
    let (binary_url, checksum_url) = select_release_assets(&release.assets)?;
    Ok(ReleaseInfo {
        version,
        binary_url,
        checksum_url,
    })
}

// exact nameのassetが1件だけ存在することを保証する
fn select_release_asset(assets: &[ReleaseAsset], expected_name: &str) -> Result<String> {
    let mut matches = assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .map(|asset| asset.browser_download_url.clone());
    let url = matches
        .next()
        .ok_or_else(|| anyhow::anyhow!("required Release asset is missing: {expected_name}"))?;
    if matches.next().is_some() {
        bail!("required Release asset is duplicated: {expected_name}");
    }
    require_https_url(&url)?;
    Ok(url)
}

// 全Public requestへtimeout・redirect・User-Agent policyを適用する
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

// Public Release通信をHTTPS URLだけへ制限する
fn require_https_url(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        bail!("Release asset URL must use HTTPS: {url}");
    }
    Ok(())
}

// installed targetが上書き可能なregular executableであることを確認する
fn validate_installed_target(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("installed Eiyah binary is unavailable: {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "installed Eiyah binary must be a regular executable file: {}",
            path.display()
        );
    }
    Ok(())
}

// stale regular fileだけを削除してsecure creation可能な状態にする
fn prepare_candidate_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::remove_file(path)?,
        Ok(_) => bail!(
            "update candidate path must be missing or a regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

// commit point前のcandidateをbest effortで削除する
fn cleanup_candidate(path: &Path) {
    let _ = fs::remove_file(path);
}

// symlinkを追跡せずpath entryの存在を確認する
fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

// regular fileにいずれかのexecute bitがあることを確認する
fn is_executable(path: &Path) -> Result<bool> {
    let metadata = fs::metadata(path)?;
    Ok(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

// state不整合として扱えるpublic entryのfilesystem errorを分類する
fn is_structural_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::InvalidData | io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
    )
}

// parse / validation errorと状態変化をPartialへ分類する
fn is_structural_failure(error: &Error) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_none_or(is_structural_error)
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::config::{InstallMetadata, save_install_metadata};

    use super::*;

    // 並列test間でtemporary directory名が衝突しないための連番
    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    // install state test専用directoryを所有するfixture
    struct TestDirectory {
        // fixtureが所有するtemporary directory path
        path: PathBuf,
    }

    impl TestDirectory {
        // process IDと連番からtest directoryを作成する
        fn new() -> Result<Self> {
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
    fn fixture_paths(root: &Path) -> Result<ResolvedPaths> {
        ResolvedPaths::from_install_metadata(InstallMetadata {
            config_home: root.join("config"),
            data_home: root.join("data"),
            state_home: root.join("state"),
            cache_home: root.join("cache"),
        })
    }

    // Installed判定に必要なbinary / entry / metadataを作成する
    fn create_installed_fixture(paths: &ResolvedPaths, public_entry: &Path) -> Result<()> {
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

    #[test]
    // managed artifactがすべてない場合にNotInstalledとなることを検証する
    fn detects_not_installed_state() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let public_entry = directory.path.join("home/.local/bin/eiyah");
        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::NotInstalled
        );
        Ok(())
    }

    #[test]
    // 整合するartifact一式をInstalledとして判定することを検証する
    fn detects_installed_state() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let public_entry = directory.path.join("home/.local/bin/eiyah");
        create_installed_fixture(&paths, &public_entry)?;
        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::Installed
        );
        Ok(())
    }

    #[test]
    // artifactが一部だけ存在する場合にPartialとなることを検証する
    fn detects_partial_state() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        fs::write(paths.eiyah_prefix.join("install.toml"), b"invalid")?;
        assert_eq!(
            detect_install_state(&paths, &directory.path.join("missing-entry"))?,
            InstallState::Partial
        );
        Ok(())
    }

    #[test]
    // install.toml pathが非regular fileの場合にPartialとなることを検証する
    fn detects_non_regular_metadata_as_partial() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let public_entry = directory.path.join("home/.local/bin/eiyah");
        create_installed_fixture(&paths, &public_entry)?;
        let metadata_path = paths.eiyah_prefix.join("install.toml");
        fs::remove_file(&metadata_path)?;
        fs::create_dir(&metadata_path)?;

        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::Partial
        );
        Ok(())
    }

    #[test]
    // broken / wrong public symlinkをPartialとして扱うことを検証する
    fn detects_invalid_public_symlinks_as_partial() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let public_entry = directory.path.join("home/.local/bin/eiyah");
        create_installed_fixture(&paths, &public_entry)?;

        fs::remove_file(&public_entry)?;
        symlink(directory.path.join("missing/bin/eiyah"), &public_entry)?;
        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::Partial
        );

        fs::remove_file(&public_entry)?;
        symlink(paths.eiyah_prefix.join("wrong/eiyah"), &public_entry)?;
        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::Partial
        );
        Ok(())
    }

    #[test]
    // invalid metadataとmetadata由来prefix不一致をPartialとして扱うことを検証する
    fn detects_invalid_or_mismatched_metadata_as_partial() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let public_entry = directory.path.join("home/.local/bin/eiyah");
        create_installed_fixture(&paths, &public_entry)?;
        let metadata_path = paths.eiyah_prefix.join("install.toml");

        fs::write(&metadata_path, b"invalid")?;
        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::Partial
        );

        let mismatched = fixture_paths(&directory.path.join("other"))?;
        fs::write(
            &metadata_path,
            toml::to_string(&InstallMetadata::from(&mismatched))?,
        )?;
        assert_eq!(
            detect_install_state(&paths, &public_entry)?,
            InstallState::Partial
        );
        Ok(())
    }
}
