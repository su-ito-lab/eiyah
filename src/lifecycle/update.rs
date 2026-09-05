// ==================================================
// @file src/lifecycle/update.rs
// @brief Public Eiyah binary update
// ==================================================

use anyhow::{Context, Result, bail};
use semver::Version;
use serde::Deserialize;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::Command;

use crate::config::{
    ResolvedPaths, discover_install_metadata, load_install_metadata, runtime_home,
};
use crate::transaction::LockGuard;

use super::{
    GITHUB_ACCEPT, GITHUB_API_VERSION, SHA256_HEX_LENGTH, http_agent, require_https_url,
    verify_checksum,
};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/su-ito-lab/eiyah/releases/latest";
const BINARY_ASSET_NAME: &str = "eiyah-x86_64-unknown-linux-gnu";
const CHECKSUM_ASSET_NAME: &str = "eiyah-x86_64-unknown-linux-gnu.sha256";
const CANDIDATE_FILE_NAME: &str = ".eiyah.new";
const CANDIDATE_DOWNLOAD_MODE: u32 = 0o600;
const CANDIDATE_EXECUTABLE_MODE: u32 = 0o755;

/// 更新に使用するstable Public Releaseの情報
#[derive(Clone, Debug, Eq, PartialEq)]
struct ReleaseInfo {
    /// Release tagから復元した更新version
    version: Version,
    /// Linux binary assetのdownload URL
    binary_url: String,
    /// checksum assetのdownload URL
    checksum_url: String,
}

#[derive(Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Clone, Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

fn fetch_latest_release() -> Result<ReleaseResponse> {
    let agent = http_agent();
    let mut response = agent
        .get(LATEST_RELEASE_URL)
        .header("Accept", GITHUB_ACCEPT)
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .call()
        .context("failed to fetch latest Eiyah release")?;
    parse_release_response(response.body_mut())
}

// GitHub response bodyから更新判定に必要なfieldだけをdecodeする
fn parse_release_response(body: &mut ureq::Body) -> Result<ReleaseResponse> {
    body.read_json()
        .context("failed to parse latest Eiyah release response")
}

/// `v<SEMVER>` 形式のRelease tagをversionへ変換する
fn parse_release_version(tag: &str) -> Result<Version> {
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
fn download_to_file(url: &str, path: &Path) -> Result<()> {
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
        .with_context(|| format!("failed to create downloaded Eiyah {}", path.display()))?;
    io::copy(&mut response.body_mut().as_reader(), &mut file)
        .with_context(|| format!("failed to write downloaded Eiyah {}", path.display()))?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

/// HTTPS assetをchecksum textとしてdownloadする
fn download_text(url: &str) -> Result<String> {
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
fn parse_checksum(text: &str) -> Result<[u8; 32]> {
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

/// Eiyah binaryの`--version` outputがexpected versionと一致することを検証する
fn validate_eiyah_binary(path: &Path, expected_version: &Version) -> Result<()> {
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
pub(crate) fn run_update() -> Result<()> {
    let home = runtime_home()?;
    let paths = load_update_paths(&home)?;
    let _lock = LockGuard::acquire(&paths.state_home)?;
    update_locked(&paths, false)
}

// Eiyah command linkから利用者向けdiagnostic付きでupdate pathを復元する
fn load_update_paths(home: &Path) -> Result<ResolvedPaths> {
    let entry = home.join(".local/bin/eiyah");
    let metadata_path = discover_install_metadata(&entry).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "Eiyah command link is missing or invalid: {}",
                entry.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })?;
    let metadata = load_install_metadata(&metadata_path).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "installation information is missing or invalid: {}",
                metadata_path.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })?;
    ResolvedPaths::from_install_metadata(metadata).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "installation paths are invalid: {}",
                metadata_path.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })
}

/// callerが保持するoperation lock内でEiyah binaryを更新する
pub(super) fn update_locked(paths: &ResolvedPaths, leading_blank: bool) -> Result<()> {
    update_locked_with(
        paths,
        leading_blank,
        fetch_latest_release,
        download_to_file,
        download_text,
    )
}

// network dependencyを差し替え可能にして更新transactionを実行する
fn update_locked_with(
    paths: &ResolvedPaths,
    leading_blank: bool,
    mut fetch_release: impl FnMut() -> Result<ReleaseResponse>,
    mut download_binary: impl FnMut(&str, &Path) -> Result<()>,
    mut download_checksum: impl FnMut(&str) -> Result<String>,
) -> Result<()> {
    let current_version =
        Version::parse(env!("CARGO_PKG_VERSION")).context("current package version is invalid")?;
    let response = fetch_release()?;
    let remote_version = validate_release_version(&response)?;
    if leading_blank {
        crate::ui::print_operation("Checking for Eiyah updates")?;
    } else {
        crate::ui::print_first_operation("Checking for Eiyah updates")?;
    }
    crate::ui::print_detail(&format!("Current: {current_version}"))?;
    crate::ui::print_detail(&format!("Latest:  {remote_version}"))?;
    if remote_version <= current_version {
        crate::ui::print_detail("")?;
        crate::ui::print_detail("Eiyah is already up to date.")?;
        return Ok(());
    }
    let release = release_info(response, remote_version)?;
    crate::ui::print_operation(&format!("Downloading Eiyah {}", release.version))?;
    crate::ui::print_detail(&release.binary_url)?;

    let binary_directory = paths.eiyah_prefix.join("bin");
    let installed = binary_directory.join("eiyah");
    validate_installed_target(&installed)?;
    let candidate = binary_directory.join(CANDIDATE_FILE_NAME);
    prepare_candidate_path(&candidate)?;

    let prepared = (|| -> Result<()> {
        download_binary(&release.binary_url, &candidate)?;
        let checksum = parse_checksum(&download_checksum(&release.checksum_url)?)?;
        verify_checksum(&candidate, &checksum)?;
        crate::ui::print_operation("Verifying Eiyah download")?;
        crate::ui::print_detail("SHA-256: verified")?;

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

    crate::ui::print_operation("Updating Eiyah")?;
    crate::ui::print_detail(&installed.display().to_string())?;
    if let Err(error) = fs::rename(&candidate, &installed) {
        cleanup_candidate(&candidate);
        return Err(error.into());
    }
    if validate_eiyah_binary(&installed, &release.version).is_err() {
        return Err(crate::ui::UserFacingError::new(
            format!(
                "Eiyah validation failed after updating to {}.",
                release.version
            ),
            vec![format!(
                "Eiyah {} is already installed and was not reverted.",
                release.version
            )],
            Vec::new(),
        )
        .into());
    }
    crate::ui::print_operation(&format!("Eiyah updated to {}", release.version))?;
    Ok(())
}

// stable flagとrequired assetから更新情報を組み立てる
fn validate_release_version(release: &ReleaseResponse) -> Result<Version> {
    if release.draft {
        bail!("latest Eiyah release is a draft");
    }
    if release.prerelease {
        bail!("latest Eiyah release is a prerelease");
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
            "downloaded Eiyah path must be missing or a regular file: {}",
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

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};

    use sha2::{Digest, Sha256};

    use crate::lifecycle::test_support::*;

    use super::*;

    // update test用のRelease assetを作成する
    fn release_asset(name: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_owned(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    // 指定versionを返す実行可能なtest binaryを作成する
    fn write_version_binary(path: &Path, version: &str) -> Result<()> {
        fs::write(
            path,
            format!("#!/bin/sh\nprintf 'eiyah {version}\\n'\n").as_bytes(),
        )?;
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(CANDIDATE_EXECUTABLE_MODE);
        fs::set_permissions(path, permissions)?;
        Ok(())
    }

    // binary contentに対応するchecksum asset textを作成する
    fn checksum_text(content: &[u8]) -> String {
        let digest = Sha256::digest(content);
        format!("{digest:x}  {BINARY_ASSET_NAME}\n")
    }

    // installed targetを持つupdate fixtureを作成する
    fn create_update_fixture(root: &Path) -> Result<ResolvedPaths> {
        let paths = fixture_paths(root)?;
        let binary = paths.eiyah_prefix.join("bin/eiyah");
        fs::create_dir_all(binary.parent().unwrap())?;
        write_version_binary(&binary, env!("CARGO_PKG_VERSION"))?;
        Ok(paths)
    }

    #[test]
    // invalid installation informationを内部用語なしのdiagnosticへ変換する
    fn reports_invalid_update_installation_information() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let entry = home.join(".local/bin/eiyah");
        create_installed_fixture(&paths, &entry)?;
        fs::write(paths.eiyah_prefix.join("install.toml"), b"invalid")?;

        let error = load_update_paths(&home).unwrap_err();
        let message = error.to_string();
        assert!(message.starts_with("installation information is missing or invalid:"));
        assert!(!message.contains("metadata"));
        Ok(())
    }

    // current package versionより大きいupdate test用versionを作成する
    fn newer_version() -> Result<Version> {
        let mut version = Version::parse(env!("CARGO_PKG_VERSION"))?;
        version.patch = version
            .patch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("package patch version cannot be incremented"))?;
        version.pre = semver::Prerelease::EMPTY;
        version.build = semver::BuildMetadata::EMPTY;
        Ok(version)
    }

    // test用のnewer Release responseを返す
    fn newer_release(version: &Version) -> ReleaseResponse {
        ReleaseResponse {
            tag_name: format!("v{version}"),
            draft: false,
            prerelease: false,
            assets: vec![
                release_asset(BINARY_ASSET_NAME),
                release_asset(CHECKSUM_ASSET_NAME),
            ],
        }
    }

    #[test]
    // stable responseからversionとrequired assetを選択することを検証する
    fn builds_release_info_from_stable_response() -> Result<()> {
        let mut body = ureq::Body::builder().data(format!(
            r#"{{"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[{{"name":"{BINARY_ASSET_NAME}","browser_download_url":"https://example.com/binary"}},{{"name":"{CHECKSUM_ASSET_NAME}","browser_download_url":"https://example.com/checksum"}}]}}"#
        ));
        let response = parse_release_response(&mut body)?;
        let version = validate_release_version(&response)?;
        let release = release_info(response, version)?;

        assert_eq!(release.version, Version::new(1, 2, 3));
        assert_eq!(release.binary_url, "https://example.com/binary");
        assert_eq!(release.checksum_url, "https://example.com/checksum");
        Ok(())
    }

    #[test]
    // draft / prerelease responseをstable Releaseとして受理しないことを検証する
    fn rejects_unstable_release_response() {
        for (draft, prerelease) in [(true, false), (false, true)] {
            assert!(
                validate_release_version(&ReleaseResponse {
                    tag_name: "v1.2.3".to_owned(),
                    draft,
                    prerelease,
                    assets: Vec::new(),
                })
                .is_err()
            );
        }
    }

    #[test]
    // malformed tagを拒否しvalid semverを保持することを検証する
    fn parses_release_versions() -> Result<()> {
        assert_eq!(parse_release_version("v1.2.3")?, Version::new(1, 2, 3));
        for tag in ["1.2.3", "v", "v1", "vv1.2.3", "v01.2.3"] {
            assert!(parse_release_version(tag).is_err(), "{tag}");
        }
        Ok(())
    }

    #[test]
    // required assetの欠落と重複を拒否することを検証する
    fn rejects_missing_or_duplicate_release_assets() {
        let binary = release_asset(BINARY_ASSET_NAME);
        let checksum = release_asset(CHECKSUM_ASSET_NAME);
        assert!(select_release_assets(std::slice::from_ref(&binary)).is_err());
        assert!(
            select_release_assets(&[binary.clone(), binary.clone(), checksum.clone()]).is_err()
        );
        assert!(select_release_assets(&[binary, checksum.clone(), checksum]).is_err());
    }

    #[test]
    // checksum exact formatとcalculated SHA-256を検証する
    fn parses_and_verifies_checksum() -> Result<()> {
        let directory = TestDirectory::new()?;
        let candidate = directory.path.join("candidate");
        let content = b"candidate binary";
        fs::write(&candidate, content)?;
        let checksum = parse_checksum(&checksum_text(content))?;
        verify_checksum(&candidate, &checksum)?;

        let mismatched = parse_checksum(&checksum_text(b"other"))?;
        assert!(verify_checksum(&candidate, &mismatched).is_err());
        Ok(())
    }

    #[test]
    // malformed checksumとfilename不一致を拒否することを検証する
    fn rejects_invalid_checksum_text() {
        let digest = "0".repeat(SHA256_HEX_LENGTH);
        for text in [
            format!("{digest} {BINARY_ASSET_NAME}\n"),
            format!("{digest}  wrong-name\n"),
            format!("{}  {BINARY_ASSET_NAME}\n", "A".repeat(SHA256_HEX_LENGTH)),
            format!("{digest}  {BINARY_ASSET_NAME}"),
            format!("{digest}  {BINARY_ASSET_NAME}\nextra\n"),
        ] {
            assert!(parse_checksum(&text).is_err(), "{text:?}");
        }
    }

    #[test]
    // stale regular candidateだけを削除しsymlinkを拒否することを検証する
    fn prepares_candidate_path_safely() -> Result<()> {
        let directory = TestDirectory::new()?;
        let candidate = directory.path.join(CANDIDATE_FILE_NAME);
        fs::write(&candidate, b"stale")?;
        prepare_candidate_path(&candidate)?;
        assert!(!candidate.exists());

        symlink(directory.path.join("target"), &candidate)?;
        assert!(prepare_candidate_path(&candidate).is_err());
        assert!(fs::symlink_metadata(&candidate)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    // candidate versionの一致・不一致を判定することを検証する
    fn validates_candidate_version() -> Result<()> {
        let directory = TestDirectory::new()?;
        let binary = directory.path.join("eiyah");
        write_version_binary(&binary, "1.2.3")?;
        validate_eiyah_binary(&binary, &Version::new(1, 2, 3))?;
        assert!(validate_eiyah_binary(&binary, &Version::new(1, 2, 4)).is_err());
        Ok(())
    }

    #[test]
    // newer binaryだけをatomic replacementしcandidateを残さないことを検証する
    fn replaces_installed_binary_atomically() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = create_update_fixture(&directory.path)?;
        let version = newer_version()?;
        let content = format!("#!/bin/sh\nprintf 'eiyah {version}\\n'\n");
        let checksum = checksum_text(content.as_bytes());

        let (result, output) = crate::ui::capture_stdout(|| {
            update_locked_with(
                &paths,
                false,
                || Ok(newer_release(&version)),
                |_, path| {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(CANDIDATE_DOWNLOAD_MODE)
                        .open(path)?;
                    file.write_all(content.as_bytes())?;
                    file.sync_all()?;
                    Ok(())
                },
                |_| Ok(checksum.clone()),
            )
        });
        result?;

        assert_eq!(
            output,
            format!(
                "==> Checking for Eiyah updates\nCurrent: {}\nLatest:  {version}\n\n==> Downloading Eiyah {version}\nhttps://example.com/{BINARY_ASSET_NAME}\n\n==> Verifying Eiyah download\nSHA-256: verified\n\n==> Updating Eiyah\n{}\n\n==> Eiyah updated to {version}\n",
                env!("CARGO_PKG_VERSION"),
                paths.eiyah_prefix.join("bin/eiyah").display(),
            )
        );

        let installed = paths.eiyah_prefix.join("bin/eiyah");
        validate_eiyah_binary(&installed, &version)?;
        assert_eq!(
            fs::symlink_metadata(&installed)?.permissions().mode() & 0o777,
            CANDIDATE_EXECUTABLE_MODE
        );
        assert!(!installed.with_file_name(CANDIDATE_FILE_NAME).exists());
        Ok(())
    }

    #[test]
    // post-update validation failureでもnew binaryを維持してrollbackしないことを検証する
    fn keeps_new_binary_after_post_update_validation_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = create_update_fixture(&directory.path)?;
        let version = newer_version()?;
        let marker = directory.path.join("candidate-validated");
        let content = format!(
            "#!/bin/sh\nif [ -e '{}' ]; then\n  printf 'eiyah {}\\n'\nelse\n  touch '{}'\n  printf 'eiyah {version}\\n'\nfi\n",
            marker.display(),
            env!("CARGO_PKG_VERSION"),
            marker.display()
        );
        let checksum = checksum_text(content.as_bytes());

        let error = update_locked_with(
            &paths,
            false,
            || Ok(newer_release(&version)),
            |_, path| {
                fs::write(path, content.as_bytes())?;
                Ok(())
            },
            |_| Ok(checksum.clone()),
        )
        .unwrap_err();

        let mut output = Vec::new();
        crate::ui::write_error_report(&mut output, &error, false)?;
        assert_eq!(
            String::from_utf8(output)?,
            format!(
                "Error: Eiyah validation failed after updating to {version}.\nWarning: Eiyah {version} is already installed and was not reverted.\n"
            )
        );

        let installed = paths.eiyah_prefix.join("bin/eiyah");
        assert_eq!(fs::read(&installed)?, content.as_bytes());
        assert!(!installed.with_file_name(CANDIDATE_FILE_NAME).exists());
        Ok(())
    }

    #[test]
    // commit point前のvalidation failureでinstalled binaryを維持しcandidateを削除する
    fn cleans_candidate_after_update_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = create_update_fixture(&directory.path)?;
        let version = newer_version()?;
        let content = format!(
            "#!/bin/sh\nprintf 'eiyah {}\\n'\n",
            env!("CARGO_PKG_VERSION")
        );
        let checksum = checksum_text(content.as_bytes());

        assert!(
            update_locked_with(
                &paths,
                false,
                || Ok(newer_release(&version)),
                |_, path| {
                    fs::write(path, content.as_bytes())?;
                    Ok(())
                },
                |_| Ok(checksum.clone()),
            )
            .is_err()
        );

        let installed = paths.eiyah_prefix.join("bin/eiyah");
        validate_eiyah_binary(&installed, &Version::parse(env!("CARGO_PKG_VERSION"))?)?;
        assert!(!installed.with_file_name(CANDIDATE_FILE_NAME).exists());
        Ok(())
    }

    #[test]
    // same / older Releaseではdownloadせずsuccessとなることを検証する
    fn skips_same_or_older_release() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let current = Version::parse(env!("CARGO_PKG_VERSION"))?;

        let (result, output) = crate::ui::capture_stdout(|| {
            update_locked_with(
                &paths,
                false,
                || {
                    Ok(ReleaseResponse {
                        tag_name: format!("v{current}"),
                        draft: false,
                        prerelease: false,
                        assets: Vec::new(),
                    })
                },
                |_, _| bail!("binary download must not run"),
                |_| bail!("checksum download must not run"),
            )
        });
        result?;
        assert_eq!(
            output,
            format!(
                "==> Checking for Eiyah updates\nCurrent: {current}\nLatest:  {current}\n\nEiyah is already up to date.\n"
            )
        );

        update_locked_with(
            &paths,
            false,
            || {
                Ok(ReleaseResponse {
                    tag_name: "v0.0.0".to_owned(),
                    draft: false,
                    prerelease: false,
                    assets: Vec::new(),
                })
            },
            |_, _| bail!("binary download must not run"),
            |_| bail!("checksum download must not run"),
        )?;
        Ok(())
    }

    #[test]
    // caller保持中のlockをupdate coreが再取得しないことを検証する
    fn update_locked_core_reuses_existing_lock() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let _lock = LockGuard::acquire(&paths.state_home)?;
        update_locked_with(
            &paths,
            false,
            || {
                Ok(ReleaseResponse {
                    tag_name: "v0.0.0".to_owned(),
                    draft: false,
                    prerelease: false,
                    assets: Vec::new(),
                })
            },
            |_, _| bail!("binary download must not run"),
            |_| bail!("checksum download must not run"),
        )
    }
}
