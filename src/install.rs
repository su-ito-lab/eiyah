// ==================================================
// @file src/install.rs
// @brief Installation state detection and binary update
// ==================================================

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

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
// GitHub App Device Flowで使用するPublic client ID
const GITHUB_CLIENT_ID: &str = "Iv23li7y3eZOlLEkxYfP";
// user access tokenをPrivate repository 1件へ限定するrepository ID
const PRIVATE_REPOSITORY_ID: &str = "1342986165";
// Private repositoryのGitHub API path
const PRIVATE_REPOSITORY: &str = "su-ito-lab/eiyah-core";
// Device codeを取得するGitHub endpoint
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
// Device Flow tokenをpollするGitHub endpoint
const DEVICE_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
// slow_down応答時に加算するpoll間隔秒数
const DEVICE_SLOW_DOWN_SECONDS: u64 = 5;
// Private Releaseから取得するshow-cad-status binary asset名
const SHOW_CAD_STATUS_ASSET_NAME: &str = "show-cad-status-x86_64-unknown-linux-gnu";
// Private Releaseから取得するshow-cad-status checksum asset名
const SHOW_CAD_STATUS_CHECKSUM_ASSET_NAME: &str = "show-cad-status-x86_64-unknown-linux-gnu.sha256";
// Eiyahが新規作成するSSH directory permission
const SSH_DIRECTORY_MODE: u32 = 0o700;
// Eiyahが新規生成するSSH private key permission
const SSH_PRIVATE_KEY_MODE: u32 = 0o600;
// Eiyahが新規生成するSSH public key permission
const SSH_PUBLIC_KEY_MODE: u32 = 0o644;
// Eiyahが新規作成するauthorized_keys permission
const AUTHORIZED_KEYS_MODE: u32 = 0o600;
// Eiyahが新規作成するinstall directory permission
const INSTALL_DIRECTORY_MODE: u32 = 0o755;
// install済みEiyah binary permission
const EIYAH_BINARY_MODE: u32 = 0o755;
// Pixi official installer URL
const PIXI_INSTALLER_URL: &str = "https://pixi.sh/install.sh";
// Pixi installerを実行するsystem Bash
const BASH_PATH: &str = "/usr/bin/bash";
// Pixi global manifest fileのpermission
const PIXI_MANIFEST_MODE: u32 = 0o644;
// authorized_keys temporary file名の衝突を避けるprocess内連番
static AUTHORIZED_KEYS_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// Private installに使用するsame-tag archiveとRelease assetの情報
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateReleaseInfo {
    /// latest stable Private Releaseのtag
    pub tag_name: String,
    /// same tagをrefに使用するrepository archive URL
    pub archive_url: String,
    /// show-cad-status binary assetのGitHub ID
    pub show_cad_status_asset_id: u64,
    /// show-cad-status checksum assetのGitHub ID
    pub show_cad_status_checksum_asset_id: u64,
}

// --------------------------------------------------
// Eiyah Installation Paths
// --------------------------------------------------

/// 初回installで作成した管理対象directoryだけを作成順で返す
pub fn create_install_directories(path: &Path) -> Result<Vec<PathBuf>> {
    create_install_directories_with(path, |_| Ok(()))
}

// 各directory作成直前のraceをtest可能にしてmissing pathを排他的に作成する
fn create_install_directories_with<F>(path: &Path, mut before_create: F) -> Result<Vec<PathBuf>>
where
    F: FnMut(&Path) -> Result<()>,
{
    let mut missing = Vec::new();
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.to_path_buf());
                current = current.parent().with_context(|| {
                    format!("install path has no existing ancestor: {}", path.display())
                })?;
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect install directory: {}", current.display())
                });
            }
        }
    }
    missing.reverse();

    let mut created = Vec::new();
    let result = (|| -> Result<()> {
        for directory in missing {
            before_create(&directory)?;
            let mut builder = DirBuilder::new();
            builder.mode(INSTALL_DIRECTORY_MODE);
            match builder.create(&directory) {
                Ok(()) => {
                    let metadata = fs::symlink_metadata(&directory).with_context(|| {
                        format!(
                            "failed to inspect created install directory: {}",
                            directory.display()
                        )
                    })?;
                    created.push((directory.clone(), metadata));
                    fs::set_permissions(
                        &directory,
                        fs::Permissions::from_mode(INSTALL_DIRECTORY_MODE),
                    )
                    .with_context(|| {
                        format!(
                            "failed to set install directory permissions: {}",
                            directory.display()
                        )
                    })?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    validate_install_directory(&directory)?;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to create install directory: {}",
                            directory.display()
                        )
                    });
                }
            }
        }
        validate_install_directory(path)
    })();

    if let Err(error) = result {
        for (directory, metadata) in created.iter().rev() {
            let _ = remove_directory_if_same_inode(directory, metadata);
        }
        return Err(error);
    }
    Ok(created.into_iter().map(|(path, _)| path).collect())
}

// managed pathが既存のnon-symlink directoryであることを保証する
fn validate_install_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect install directory: {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "install path must be a non-symlink directory: {}",
            path.display()
        );
    }
    Ok(())
}

// pathが作成時と同じdirectory inodeを指す場合だけcleanupする
fn remove_directory_if_same_inode(path: &Path, created: &fs::Metadata) -> io::Result<()> {
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if same_inode(&current, created) {
        fs::remove_dir(path)?;
    }
    Ok(())
}

/// 実行中のEiyah binaryを初回install targetへ配置する
pub fn install_running_eiyah_binary(paths: &ResolvedPaths) -> Result<()> {
    let source = env::current_exe().context("failed to resolve running Eiyah executable")?;
    install_eiyah_binary_from(paths, &source)
}

// 指定sourceを使用してbinary配置contractを検証する
fn install_eiyah_binary_from(paths: &ResolvedPaths, source: &Path) -> Result<()> {
    install_eiyah_binary_with(paths, source, |source, target| io::copy(source, target))
}

// copy失敗時のcleanupを含めてEiyah binaryを配置する
fn install_eiyah_binary_with<F>(paths: &ResolvedPaths, source: &Path, copy: F) -> Result<()>
where
    F: FnOnce(&mut File, &mut File) -> io::Result<u64>,
{
    validate_source_binary(source)?;
    let target = paths.eiyah_prefix.join("bin/eiyah");
    let mut source_file = File::open(source)
        .with_context(|| format!("failed to open source Eiyah binary: {}", source.display()))?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(EIYAH_BINARY_MODE)
        .open(&target)
        .with_context(|| format!("failed to create Eiyah binary: {}", target.display()))?;
    // cleanup対象を今回create_newしたinodeへ限定するためのidentity
    let created_target = target_file
        .metadata()
        .with_context(|| format!("failed to inspect Eiyah binary: {}", target.display()))?;

    let result = (|| -> Result<()> {
        target_file
            .set_permissions(fs::Permissions::from_mode(EIYAH_BINARY_MODE))
            .with_context(|| format!("failed to set Eiyah binary mode: {}", target.display()))?;
        copy(&mut source_file, &mut target_file)
            .with_context(|| format!("failed to copy Eiyah binary: {}", target.display()))?;
        target_file
            .sync_all()
            .with_context(|| format!("failed to sync Eiyah binary: {}", target.display()))
    })();

    if result.is_err() {
        drop(target_file);
        let _ = remove_file_if_same_inode(&target, &created_target);
    }
    result
}

// pathが作成時と同じinodeを指す場合だけpartial fileをcleanupする
fn remove_file_if_same_inode(path: &Path, created: &fs::Metadata) -> io::Result<()> {
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if same_inode(&current, created) {
        fs::remove_file(path)?;
    }
    Ok(())
}

// filesystem deviceとinodeで今回作成したpathを識別する
fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

// sourceがsymlinkでない実行可能なregular fileであることを保証する
fn validate_source_binary(source: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).with_context(|| {
        format!(
            "failed to inspect source Eiyah binary: {}",
            source.display()
        )
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!(
            "source Eiyah binary must be a regular file: {}",
            source.display()
        );
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!(
            "source Eiyah binary must be executable: {}",
            source.display()
        );
    }
    Ok(())
}

/// HOME配下へinstalled binaryを指すabsolute public symlinkを作成する
pub fn create_eiyah_public_entry(paths: &ResolvedPaths, home: &Path) -> Result<PathBuf> {
    let target = paths.eiyah_prefix.join("bin/eiyah");
    if !target.is_absolute() {
        bail!("public entry target must be absolute: {}", target.display());
    }
    let public_entry = home.join(".local/bin/eiyah");
    symlink(&target, &public_entry).with_context(|| {
        format!(
            "failed to create public Eiyah entry: {}",
            public_entry.display()
        )
    })?;
    Ok(public_entry)
}

// --------------------------------------------------
// Pixi Bootstrap
// --------------------------------------------------

/// official installerとPrivate manifestからEiyah専用Pixi環境を構築する
pub fn bootstrap_pixi(paths: &ResolvedPaths, core_root: &Path) -> Result<()> {
    let home = runtime_home()?;
    bootstrap_pixi_with(
        paths,
        core_root,
        &home,
        download_pixi_installer,
        |_| Ok(()),
        run_pixi_installer,
        |command| command.output().map_err(Into::into),
        |command| command.status().map_err(Into::into),
    )
}

// external I/Oを差し替え可能にしてPixi bootstrap contractを実行する
fn bootstrap_pixi_with<Download, BeforeHome, Installer, Version, Sync>(
    paths: &ResolvedPaths,
    core_root: &Path,
    home: &Path,
    download: Download,
    before_home_create: BeforeHome,
    mut install: Installer,
    mut version: Version,
    mut sync: Sync,
) -> Result<()>
where
    Download: FnOnce(&str) -> Result<Vec<u8>>,
    BeforeHome: FnOnce(&Path) -> Result<()>,
    Installer: FnMut(&mut Command, &[u8]) -> Result<ExitStatus>,
    Version: FnMut(&mut Command) -> Result<Output>,
    Sync: FnMut(&mut Command) -> Result<ExitStatus>,
{
    let installer_script = download(PIXI_INSTALLER_URL)?;
    if installer_script.is_empty() {
        bail!("Pixi installer script is empty");
    }

    before_home_create(&paths.pixi_home)?;
    let pixi_home_metadata = create_pixi_home(&paths.pixi_home)?;
    let mut sync_started = false;
    let mut unowned_manifest_present = false;
    let result = (|| -> Result<()> {
        let mut installer = pixi_installer_command(&paths.pixi_home);
        let status = install(&mut installer, &installer_script)
            .context("failed to execute Pixi installer")?;
        if !status.success() {
            bail!("Pixi installer failed: {status}");
        }

        let pixi_binary = paths.pixi_home.join("bin/pixi");
        validate_pixi_binary_with(&pixi_binary, &mut version)?;

        let source_manifest = core_root.join("pixi/pixi-global.toml");
        validate_pixi_manifest_source(&source_manifest)?;
        let manifests = paths.pixi_home.join("manifests");
        create_install_directories(&manifests)?;
        let target_manifest = manifests.join("pixi-global.toml");
        if let Err(error) = place_pixi_manifest(&source_manifest, &target_manifest) {
            unowned_manifest_present = path_exists(&target_manifest)?;
            return Err(error);
        }

        let mut command = pixi_sync_command(&pixi_binary, &paths.pixi_home, home);
        sync_started = true;
        let status = sync(&mut command).context("failed to execute pixi global sync")?;
        if !status.success() {
            bail!("pixi global sync failed: {status}");
        }
        Ok(())
    })();

    if result.is_err() && !sync_started && !unowned_manifest_present {
        let _ = remove_owned_tree(&paths.pixi_home, &pixi_home_metadata);
    }
    result
}

// HTTPSからofficial installerをmemoryへ取得する
fn download_pixi_installer(url: &str) -> Result<Vec<u8>> {
    require_https_url(url)?;
    let agent = http_agent();
    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("failed to download Pixi installer: {url}"))?;
    let mut script = Vec::new();
    io::copy(&mut response.body_mut().as_reader(), &mut script)
        .context("failed to read Pixi installer")?;
    Ok(script)
}

// PIXI_HOMEを排他的に作成してcleanup用identityを返す
fn create_pixi_home(path: &Path) -> Result<fs::Metadata> {
    let mut builder = DirBuilder::new();
    builder.mode(INSTALL_DIRECTORY_MODE);
    builder
        .create(path)
        .with_context(|| format!("failed to create PIXI_HOME: {}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect PIXI_HOME: {}", path.display()))?;
    if let Err(error) =
        fs::set_permissions(path, fs::Permissions::from_mode(INSTALL_DIRECTORY_MODE))
    {
        let _ = remove_owned_tree(path, &metadata);
        return Err(error)
            .with_context(|| format!("failed to set PIXI_HOME permissions: {}", path.display()));
    }
    Ok(metadata)
}

// identityが一致する今回作成分のtreeだけをcleanupする
fn remove_owned_tree(path: &Path, created: &fs::Metadata) -> io::Result<()> {
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if same_inode(&current, created) {
        fs::remove_dir_all(path)?;
    }
    Ok(())
}

// installerへ固定environmentを渡しoverride用environmentを除去する
fn pixi_installer_command(pixi_home: &Path) -> Command {
    let mut command = Command::new(BASH_PATH);
    command
        .env("PIXI_HOME", pixi_home)
        .env("PIXI_NO_PATH_UPDATE", "1");
    for name in [
        "PIXI_BIN_DIR",
        "PIXI_VERSION",
        "PIXI_ARCH",
        "PIXI_DOWNLOAD_URL",
        "PIXI_CACHE_DIR",
        "RATTLER_CACHE_DIR",
        "NETRC",
        "TMP_DIR",
    ] {
        command.env_remove(name);
    }
    command
}

// installer scriptをstdinへ渡してclose後に終了statusを待つ
fn run_pixi_installer(command: &mut Command, script: &[u8]) -> Result<ExitStatus> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("failed to open Pixi installer stdin"))?;
    let write_result = stdin.write_all(script);
    drop(stdin);
    let status = child.wait();
    write_result?;
    Ok(status?)
}

// expected Pixi binaryの形状とversion実行結果を検証する
fn validate_pixi_binary_with(
    path: &Path,
    execute: &mut impl FnMut(&mut Command) -> Result<Output>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Pixi binary is unavailable: {}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o111 == 0
    {
        bail!(
            "Pixi binary must be a regular executable file: {}",
            path.display()
        );
    }
    let mut command = Command::new(path);
    command.arg("--version");
    let output = execute(&mut command)
        .with_context(|| format!("failed to execute Pixi binary: {}", path.display()))?;
    if !output.status.success() || output.stdout.is_empty() {
        bail!("Pixi binary validation failed: {}", path.display());
    }
    Ok(())
}

// Private root配下のmanifest sourceがnon-symlink regular fileであることを保証する
fn validate_pixi_manifest_source(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("Pixi manifest source is unavailable: {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!(
            "Pixi manifest source must be a regular file: {}",
            path.display()
        );
    }
    Ok(())
}

// Private manifestをinitial targetへbyte-for-byteで配置する
fn place_pixi_manifest(source: &Path, target: &Path) -> Result<()> {
    place_pixi_manifest_with(source, target, |source, target| {
        io::copy(source, target).map(|_| ())
    })
}

// target作成直前のraceとcopy failureをtest可能にしてmanifestを配置する
fn place_pixi_manifest_with<F>(source: &Path, target: &Path, mut copy: F) -> Result<()>
where
    F: FnMut(&mut File, &mut File) -> io::Result<()>,
{
    validate_pixi_manifest_source(source)?;
    let mut source_file = File::open(source)?;
    let mut target_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(PIXI_MANIFEST_MODE)
        .open(target)
        .with_context(|| format!("failed to create Pixi manifest: {}", target.display()))?;
    let created = target_file.metadata()?;
    let result = (|| -> Result<()> {
        target_file.set_permissions(fs::Permissions::from_mode(PIXI_MANIFEST_MODE))?;
        copy(&mut source_file, &mut target_file)?;
        target_file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        drop(target_file);
        let _ = remove_file_if_same_inode(target, &created);
    }
    result
}

// Pixi global syncをexpected binary・manifest home・working directoryへ固定する
fn pixi_sync_command(pixi: &Path, pixi_home: &Path, home: &Path) -> Command {
    let mut command = Command::new(pixi);
    command
        .arg("global")
        .arg("sync")
        .current_dir(home)
        .env("PIXI_HOME", pixi_home)
        .env("PIXI_NO_PATH_UPDATE", "1")
        .env_remove("PIXI_BIN_DIR")
        .env_remove("PIXI_CACHE_DIR")
        .env_remove("RATTLER_CACHE_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command
}

// GitHub Device code response
#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    // token pollingに使用するsecret code
    device_code: String,
    // userがGitHubへ入力するcode
    user_code: String,
    // userが認証操作を行うURL
    verification_uri: String,
    // device codeが失効するまでの秒数
    expires_in: u64,
    // GitHubが指定するpoll間隔秒数
    interval: u64,
}

// GitHub Device token polling response
#[derive(Debug, Deserialize)]
struct DeviceTokenResponse {
    // 認証成功時のuser access token
    access_token: Option<String>,
    // polling継続または失敗理由
    error: Option<String>,
    // GitHubが返す具体的なerror説明
    error_description: Option<String>,
}

// Device token responseから決定したpoll action
#[derive(Debug, Eq, PartialEq)]
enum DevicePollAction {
    // 認証済みtokenを返す
    Authorized(String),
    // 現在の間隔でpollを継続する
    Pending,
    // poll間隔を増やして継続する
    SlowDown,
}

// Private latest Release responseで使用するfield
#[derive(Debug, Deserialize)]
struct PrivateReleaseResponse {
    // Release tag
    tag_name: String,
    // draft Releaseかを示すflag
    draft: bool,
    // prereleaseかを示すflag
    prerelease: bool,
    // Releaseに添付されたasset
    assets: Vec<PrivateReleaseAsset>,
}

// Private required asset選択に使用するGitHub field
#[derive(Clone, Debug, Deserialize)]
struct PrivateReleaseAsset {
    // assetのexact name
    name: String,
    // authenticated asset downloadに使用するGitHub ID
    id: u64,
}

// OpenSSH public key lineから比較に必要なfieldを保持する
#[derive(Clone, Debug, Eq, PartialEq)]
struct SshPublicKey {
    // key algorithmを表す先頭field
    key_type: Vec<u8>,
    // encoded public key dataを表す第2 field
    key_data: Vec<u8>,
    // authorized_keysへ追加できるtrim済みoriginal line
    line: Vec<u8>,
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

/// GitHub Device FlowでPrivate repository用user access tokenを取得する
pub fn authorize_private_repository() -> Result<String> {
    let agent = http_agent();
    let mut response = agent
        .post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .send_form([("client_id", GITHUB_CLIENT_ID)])
        .context("failed to request GitHub device code")?;
    let issued_at = Instant::now();
    let device: DeviceCodeResponse = response
        .body_mut()
        .read_json()
        .context("failed to parse GitHub device code response")?;
    write_device_instructions(&mut io::stdout().lock(), &device)?;
    poll_device_token(&agent, &device, issued_at)
}

/// authenticated GitHub APIからlatest stable Private Release情報を取得する
pub fn fetch_private_release(access_token: &str) -> Result<PrivateReleaseInfo> {
    let url = format!("https://api.github.com/repos/{PRIVATE_REPOSITORY}/releases/latest");
    let agent = http_agent();
    let mut response = private_request(&agent, &url, access_token)
        .call()
        .context("failed to fetch latest Private Release")?;
    let release = parse_private_release_response(response.body_mut())?;
    private_release_info(release)
}

// Private Release responseから取得に必要なfieldだけをdecodeする
fn parse_private_release_response(body: &mut ureq::Body) -> Result<PrivateReleaseResponse> {
    body.read_json()
        .context("failed to parse latest Private Release response")
}

// Device Flowのuser向けinstructionだけをstdoutへ出力する
fn write_device_instructions(output: &mut impl Write, device: &DeviceCodeResponse) -> Result<()> {
    writeln!(output, "==> Authorize Eiyah with GitHub")?;
    writeln!(output, "Open: {}", device.verification_uri)?;
    writeln!(output, "Code: {}", device.user_code)?;
    Ok(())
}

// GitHub指定のintervalとdeadlineを守ってDevice tokenをpollする
fn poll_device_token(
    agent: &ureq::Agent,
    device: &DeviceCodeResponse,
    issued_at: Instant,
) -> Result<String> {
    let deadline = issued_at
        .checked_add(Duration::from_secs(device.expires_in))
        .ok_or_else(|| anyhow::anyhow!("GitHub device code expiry is out of range"))?;
    let mut interval = Duration::from_secs(device.interval);

    loop {
        ensure_next_poll_before_deadline(Instant::now(), interval, deadline)?;
        thread::sleep(interval);

        let mut response = agent
            .post(DEVICE_TOKEN_URL)
            .header("Accept", "application/json")
            .send_form([
                ("client_id", GITHUB_CLIENT_ID),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("repository_id", PRIVATE_REPOSITORY_ID),
            ])
            .context("failed to poll GitHub Device Flow token")?;
        let token: DeviceTokenResponse = response
            .body_mut()
            .read_json()
            .context("failed to parse GitHub Device Flow token response")?;
        if Instant::now() >= deadline {
            bail!("GitHub device code expired");
        }
        match device_poll_action(token)? {
            DevicePollAction::Authorized(access_token) => return Ok(access_token),
            DevicePollAction::Pending => {}
            DevicePollAction::SlowDown => {
                interval = increase_device_poll_interval(interval)?;
            }
        }
    }
}

// slow_down応答に従ってDevice tokenのpoll間隔を増やす
fn increase_device_poll_interval(interval: Duration) -> Result<Duration> {
    interval
        .checked_add(Duration::from_secs(DEVICE_SLOW_DOWN_SECONDS))
        .ok_or_else(|| anyhow::anyhow!("GitHub Device Flow interval is out of range"))
}

// 次回pollがDevice codeのdeadlineより前に開始できることを確認する
fn ensure_next_poll_before_deadline(
    now: Instant,
    interval: Duration,
    deadline: Instant,
) -> Result<()> {
    let next_poll = now
        .checked_add(interval)
        .ok_or_else(|| anyhow::anyhow!("GitHub Device Flow interval is out of range"))?;
    if next_poll >= deadline {
        bail!("GitHub device code expired");
    }
    Ok(())
}

// Device token responseを成功・継続・terminal errorへ分類する
fn device_poll_action(response: DeviceTokenResponse) -> Result<DevicePollAction> {
    if let Some(access_token) = response.access_token {
        if access_token.is_empty() {
            bail!("GitHub Device Flow returned an empty access token");
        }
        return Ok(DevicePollAction::Authorized(access_token));
    }

    let error = response
        .error
        .ok_or_else(|| anyhow::anyhow!("GitHub Device Flow response has no token or error"))?;
    match error.as_str() {
        "authorization_pending" => Ok(DevicePollAction::Pending),
        "slow_down" => Ok(DevicePollAction::SlowDown),
        "expired_token" => bail!("GitHub device code expired"),
        "access_denied" => bail!("GitHub authorization was denied"),
        _ => bail!(
            "GitHub Device Flow error: {}",
            response.error_description.as_deref().unwrap_or(&error)
        ),
    }
}

// Private REST APIへ共通headerとBearer tokenを適用する
fn private_request<'a>(
    agent: &'a ureq::Agent,
    url: &'a str,
    access_token: &str,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    agent
        .get(url)
        .header("Accept", GITHUB_ACCEPT)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
}

// stable Releaseとrequired assetからPrivate取得情報を組み立てる
fn private_release_info(release: PrivateReleaseResponse) -> Result<PrivateReleaseInfo> {
    if release.draft {
        bail!("latest Private Release is a draft");
    }
    if release.prerelease {
        bail!("latest Private Release is a prerelease");
    }
    if release.tag_name.is_empty() {
        bail!("latest Private Release tag is empty");
    }
    let show_cad_status_asset_id =
        select_private_release_asset(&release.assets, SHOW_CAD_STATUS_ASSET_NAME)?;
    let show_cad_status_checksum_asset_id =
        select_private_release_asset(&release.assets, SHOW_CAD_STATUS_CHECKSUM_ASSET_NAME)?;
    let archive_url = format!(
        "https://api.github.com/repos/{PRIVATE_REPOSITORY}/tarball/{}",
        release.tag_name
    );
    Ok(PrivateReleaseInfo {
        tag_name: release.tag_name,
        archive_url,
        show_cad_status_asset_id,
        show_cad_status_checksum_asset_id,
    })
}

// exact nameのPrivate Release asset IDが1件だけ存在することを保証する
fn select_private_release_asset(
    assets: &[PrivateReleaseAsset],
    expected_name: &str,
) -> Result<u64> {
    let mut matches = assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .map(|asset| asset.id);
    let id = matches.next().ok_or_else(|| {
        anyhow::anyhow!("required Private Release asset is missing: {expected_name}")
    })?;
    if matches.next().is_some() {
        bail!("required Private Release asset is duplicated: {expected_name}");
    }
    Ok(id)
}

/// `$HOME`配下のed25519 key pairと`authorized_keys`を準備する
pub fn bootstrap_ssh(home: &Path) -> Result<()> {
    let user = env::var_os("USER").filter(|value| !value.is_empty());
    bootstrap_ssh_with(home, user.as_deref(), |command| command.output())
}

// ssh-keygen実行を差し替え可能にしてSSH bootstrapを行う
fn bootstrap_ssh_with(
    home: &Path,
    user: Option<&OsStr>,
    mut execute: impl FnMut(&mut Command) -> io::Result<Output>,
) -> Result<()> {
    let ssh_directory = home.join(".ssh");
    ensure_ssh_directory(&ssh_directory)?;
    let private_key = ssh_directory.join("id_ed25519");
    let public_key = ssh_directory.join("id_ed25519.pub");
    let authorized_keys = ssh_directory.join("authorized_keys");
    validate_optional_regular_file(&private_key)?;
    validate_optional_regular_file(&public_key)?;
    validate_optional_regular_file(&authorized_keys)?;

    let private_exists = path_exists(&private_key)?;
    let public_exists = path_exists(&public_key)?;
    let key = match (private_exists, public_exists) {
        (true, true) => {
            let derived = derive_public_key(&private_key, &mut execute)?;
            let existing = parse_public_key(&fs::read(&public_key)?)?;
            if !same_public_key(&derived, &existing) {
                bail!("SSH private and public keys do not match");
            }
            existing
        }
        (true, false) => {
            let derived = derive_public_key(&private_key, &mut execute)?;
            write_new_public_key(&public_key, &derived)?;
            derived
        }
        (false, true) => bail!("SSH public key exists without its private key"),
        (false, false) => {
            let user = user.ok_or_else(|| anyhow::anyhow!("USER is unavailable"))?;
            let mut key_pair_created = false;
            let generated = (|| -> Result<SshPublicKey> {
                generate_key_pair(&private_key, user, &mut execute)?;
                key_pair_created = true;
                validate_required_regular_file(&private_key)?;
                validate_required_regular_file(&public_key)?;
                set_file_mode(&private_key, SSH_PRIVATE_KEY_MODE)?;
                set_file_mode(&public_key, SSH_PUBLIC_KEY_MODE)?;
                parse_public_key(&fs::read(&public_key)?)
            })();
            if generated.is_err() && key_pair_created {
                cleanup_generated_key_pair(&private_key);
            }
            generated?
        }
    };

    update_authorized_keys(&authorized_keys, &key)
}

// `.ssh`を検証しmissing時だけ規定permissionで作成する
fn ensure_ssh_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => bail!(
            "SSH path must be a non-symlink directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.mode(SSH_DIRECTORY_MODE).create(path)?;
            set_file_mode(path, SSH_DIRECTORY_MODE)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

// optional SSH fileが存在する場合にregular non-symlinkであることを確認する
fn validate_optional_regular_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => bail!(
            "SSH path must be a non-symlink regular file: {}",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

// ssh-keygenが作成すべきfileの形状を確認する
fn validate_required_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("SSH key was not created: {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!(
            "SSH key must be a non-symlink regular file: {}",
            path.display()
        );
    }
    Ok(())
}

// private keyからOpenSSH public keyを導出する
fn derive_public_key(
    private_key: &Path,
    execute: &mut impl FnMut(&mut Command) -> io::Result<Output>,
) -> Result<SshPublicKey> {
    let mut command = Command::new("ssh-keygen");
    command
        .arg("-y")
        .arg("-f")
        .arg(private_key)
        .stdin(Stdio::null());
    let output = execute(&mut command).context("failed to execute ssh-keygen -y")?;
    if !output.status.success() {
        bail!(
            "ssh-keygen -y failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_public_key(&output.stdout)
}

// ed25519 key pairをnon-interactiveに生成する
fn generate_key_pair(
    private_key: &Path,
    user: &OsStr,
    execute: &mut impl FnMut(&mut Command) -> io::Result<Output>,
) -> Result<()> {
    let mut comment = OsString::from(user);
    comment.push("@cad");
    let mut command = Command::new("ssh-keygen");
    command
        .arg("-t")
        .arg("ed25519")
        .arg("-f")
        .arg(private_key)
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg(comment)
        .stdin(Stdio::null());
    let output = execute(&mut command).context("failed to execute ssh-keygen")?;
    if !output.status.success() {
        bail!(
            "ssh-keygen failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ssh-keygen成功後に確認済みの新規key pairをbest effortで削除する
fn cleanup_generated_key_pair(private_key: &Path) {
    let _ = fs::remove_file(private_key);
    let _ = fs::remove_file(private_key.with_extension("pub"));
}

// OpenSSH public key grammarから比較fieldと保存lineを復元する
fn parse_public_key(content: &[u8]) -> Result<SshPublicKey> {
    let line = trim_ascii_whitespace(content);
    if line.is_empty() || line.contains(&b'\n') || line.contains(&b'\r') {
        bail!("SSH public key must contain exactly one valid line");
    }
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let key_type = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("SSH public key type is missing"))?;
    let key_data = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("SSH public key data is missing"))?;
    Ok(SshPublicKey {
        key_type: key_type.to_vec(),
        key_data: key_data.to_vec(),
        line: line.to_vec(),
    })
}

// public keyのtypeとencoded dataだけを比較する
fn same_public_key(left: &SshPublicKey, right: &SshPublicKey) -> bool {
    left.key_type == right.key_type && left.key_data == right.key_data
}

// private keyから導出したpublic keyをcreate_newで保存する
fn write_new_public_key(path: &Path, key: &SshPublicKey) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SSH_PUBLIC_KEY_MODE)
        .open(path)?;
    let result = (|| -> Result<()> {
        set_file_mode(path, SSH_PUBLIC_KEY_MODE)?;
        file.write_all(&key.line)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

// authorized_keysへ同一keyがない場合だけatomic replacementで追加する
fn update_authorized_keys(path: &Path, key: &SshPublicKey) -> Result<()> {
    let existing = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    if existing
        .split(|byte| *byte == b'\n')
        .filter_map(|line| parse_public_key(line).ok())
        .any(|candidate| same_public_key(&candidate, key))
    {
        return Ok(());
    }

    let mode = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.permissions().mode() & 0o7777,
        Err(error) if error.kind() == io::ErrorKind::NotFound => AUTHORIZED_KEYS_MODE,
        Err(error) => return Err(error.into()),
    };
    let temporary = authorized_keys_temporary_path(path);
    replace_authorized_keys(path, &temporary, &existing, key, mode)
}

// 指定temporary pathを使用してauthorized_keysをatomic replacementする
fn replace_authorized_keys(
    path: &Path,
    temporary: &Path,
    existing: &[u8],
    key: &SshPublicKey,
    mode: u32,
) -> Result<()> {
    let mut temporary_created = false;
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(temporary)?;
        temporary_created = true;
        set_file_mode(temporary, mode)?;
        file.write_all(existing)?;
        if !existing.is_empty() && !existing.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        file.write_all(&key.line)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(temporary, path)?;
        Ok(())
    })();
    if result.is_err() && temporary_created {
        let _ = fs::remove_file(temporary);
    }
    result
}

// authorized_keysと同じdirectoryへprocess固有のtemporary pathを割り当てる
fn authorized_keys_temporary_path(path: &Path) -> std::path::PathBuf {
    let sequence = AUTHORIZED_KEYS_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_file_name(format!(
        ".authorized_keys.eiyah.{}.{sequence}",
        std::process::id()
    ))
}

// byte列のleading / trailing ASCII whitespaceを除く
fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

// Eiyahが作成したSSH fileへ規定permissionを適用する
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::symlink_metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
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
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::process::ExitStatusExt;
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

    // Private Release test用assetを作成する
    fn private_release_asset(name: &str, id: u64) -> PrivateReleaseAsset {
        PrivateReleaseAsset {
            name: name.to_owned(),
            id,
        }
    }

    // required assetを持つstable Private Release responseを作成する
    fn stable_private_release() -> PrivateReleaseResponse {
        PrivateReleaseResponse {
            tag_name: "v1.2.3".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![
                private_release_asset(SHOW_CAD_STATUS_ASSET_NAME, 101),
                private_release_asset(SHOW_CAD_STATUS_CHECKSUM_ASSET_NAME, 102),
            ],
        }
    }

    // ssh-keygen test doubleが返すprocess outputを作成する
    fn ssh_keygen_output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    // SSH bootstrap test用HOME directoryを作成する
    fn ssh_home(directory: &TestDirectory) -> Result<PathBuf> {
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        Ok(home)
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

        update_locked_with(
            &paths,
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
        )?;

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

        assert!(
            update_locked_with(
                &paths,
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

        for version in [current.clone(), Version::new(0, 0, 0)] {
            update_locked_with(
                &paths,
                || {
                    Ok(ReleaseResponse {
                        tag_name: format!("v{version}"),
                        draft: false,
                        prerelease: false,
                        assets: Vec::new(),
                    })
                },
                |_, _| bail!("binary download must not run"),
                |_| bail!("checksum download must not run"),
            )?;
        }
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

    #[test]
    // Device code responseとuser向け表示がsecretを含まないことを検証する
    fn parses_and_displays_device_code_response() -> Result<()> {
        let mut body = ureq::Body::builder().data(
            r#"{"device_code":"secret-device","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
        );
        let device: DeviceCodeResponse = body.read_json()?;
        let mut output = Vec::new();
        write_device_instructions(&mut output, &device)?;
        let output = String::from_utf8(output)?;

        assert_eq!(device.expires_in, 900);
        assert_eq!(device.interval, 5);
        assert_eq!(
            output,
            "==> Authorize Eiyah with GitHub\nOpen: https://github.com/login/device\nCode: ABCD-1234\n"
        );
        assert!(!output.contains(&device.device_code));
        Ok(())
    }

    #[test]
    // Device code deadline到達時には次回pollを開始しないことを検証する
    fn rejects_poll_at_or_after_device_deadline() -> Result<()> {
        let now = Instant::now();
        let interval = Duration::from_secs(5);
        ensure_next_poll_before_deadline(now, interval, now + Duration::from_secs(6))?;
        assert!(ensure_next_poll_before_deadline(now, interval, now + interval).is_err());
        assert!(ensure_next_poll_before_deadline(now, interval, now).is_err());
        Ok(())
    }

    #[test]
    // slow_down時にpoll間隔へ規定の5秒を加算することを検証する
    fn increases_device_poll_interval_after_slow_down() -> Result<()> {
        assert_eq!(
            increase_device_poll_interval(Duration::from_secs(5))?,
            Duration::from_secs(10)
        );
        Ok(())
    }

    #[test]
    // Device token responseをsuccess・継続・terminal errorへ分類することを検証する
    fn classifies_device_token_responses() -> Result<()> {
        assert_eq!(
            device_poll_action(DeviceTokenResponse {
                access_token: Some("token".to_owned()),
                error: None,
                error_description: None,
            })?,
            DevicePollAction::Authorized("token".to_owned())
        );
        for (error, expected) in [
            ("authorization_pending", DevicePollAction::Pending),
            ("slow_down", DevicePollAction::SlowDown),
        ] {
            assert_eq!(
                device_poll_action(DeviceTokenResponse {
                    access_token: None,
                    error: Some(error.to_owned()),
                    error_description: None,
                })?,
                expected
            );
        }
        for error in ["expired_token", "access_denied", "unexpected"] {
            assert!(
                device_poll_action(DeviceTokenResponse {
                    access_token: None,
                    error: Some(error.to_owned()),
                    error_description: Some("detail".to_owned()),
                })
                .is_err(),
                "{error}"
            );
        }
        assert!(
            device_poll_action(DeviceTokenResponse {
                access_token: Some(String::new()),
                error: None,
                error_description: None,
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    // stable Private Releaseからsame-tag archiveとrequired asset IDを選択する
    fn builds_private_release_info() -> Result<()> {
        let mut body = ureq::Body::builder().data(format!(
            r#"{{"tag_name":"v1.2.3","draft":false,"prerelease":false,"assets":[{{"name":"{SHOW_CAD_STATUS_ASSET_NAME}","id":101}},{{"name":"{SHOW_CAD_STATUS_CHECKSUM_ASSET_NAME}","id":102}}]}}"#
        ));
        let info = private_release_info(parse_private_release_response(&mut body)?)?;
        assert_eq!(info.tag_name, "v1.2.3");
        assert_eq!(
            info.archive_url,
            "https://api.github.com/repos/su-ito-lab/eiyah-core/tarball/v1.2.3"
        );
        assert_eq!(info.show_cad_status_asset_id, 101);
        assert_eq!(info.show_cad_status_checksum_asset_id, 102);
        Ok(())
    }

    #[test]
    // unstable Private Releaseとmissing・duplicate required assetを拒否する
    fn rejects_invalid_private_release() {
        for (draft, prerelease) in [(true, false), (false, true)] {
            let mut release = stable_private_release();
            release.draft = draft;
            release.prerelease = prerelease;
            assert!(private_release_info(release).is_err());
        }

        let mut missing = stable_private_release();
        missing.assets.pop();
        assert!(private_release_info(missing).is_err());

        let mut duplicate = stable_private_release();
        duplicate.assets.push(private_release_asset(
            SHOW_CAD_STATUS_CHECKSUM_ASSET_NAME,
            103,
        ));
        assert!(private_release_info(duplicate).is_err());

        let mut empty_tag = stable_private_release();
        empty_tag.tag_name.clear();
        assert!(private_release_info(empty_tag).is_err());
    }

    #[test]
    // existing key pairを照合し同一authorized keyを重複追加しないことを検証する
    fn reuses_matching_ssh_key_pair() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        fs::create_dir(&ssh)?;
        fs::write(ssh.join("id_ed25519"), b"private")?;
        fs::write(
            ssh.join("id_ed25519.pub"),
            b"ssh-ed25519 AAAA existing-comment\n",
        )?;
        let authorized = b"  ssh-ed25519   AAAA other-comment  \n";
        fs::write(ssh.join("authorized_keys"), authorized)?;

        bootstrap_ssh_with(&home, Some(OsStr::new("user")), |command| {
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    OsStr::new("-y"),
                    OsStr::new("-f"),
                    ssh.join("id_ed25519").as_os_str()
                ]
            );
            Ok(ssh_keygen_output(0, b"ssh-ed25519 AAAA derived\n", b""))
        })?;

        assert_eq!(fs::read(ssh.join("authorized_keys"))?, authorized);
        Ok(())
    }

    #[test]
    // private keyからmissing public keyを導出してauthorized_keysへ追加する
    fn derives_missing_ssh_public_key() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        fs::create_dir(&ssh)?;
        fs::write(ssh.join("id_ed25519"), b"private")?;

        bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| {
            Ok(ssh_keygen_output(
                0,
                b" ssh-ed25519 AAAA derived-comment \n",
                b"",
            ))
        })?;

        assert_eq!(
            fs::read(ssh.join("id_ed25519.pub"))?,
            b"ssh-ed25519 AAAA derived-comment\n"
        );
        assert_eq!(
            fs::metadata(ssh.join("id_ed25519.pub"))?
                .permissions()
                .mode()
                & 0o777,
            SSH_PUBLIC_KEY_MODE
        );
        assert_eq!(
            fs::read(ssh.join("authorized_keys"))?,
            b"ssh-ed25519 AAAA derived-comment\n"
        );
        assert_eq!(
            fs::metadata(ssh.join("authorized_keys"))?
                .permissions()
                .mode()
                & 0o777,
            AUTHORIZED_KEYS_MODE
        );
        Ok(())
    }

    #[test]
    // key pairをnon-interactive argvとnon-UTF-8 USER commentで新規生成する
    fn generates_new_ssh_key_pair() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        let user = OsString::from_vec(vec![b'u', 0x80]);

        bootstrap_ssh_with(&home, Some(&user), |command| {
            let private = ssh.join("id_ed25519");
            let mut comment = user.clone();
            comment.push("@cad");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    OsStr::new("-t"),
                    OsStr::new("ed25519"),
                    OsStr::new("-f"),
                    private.as_os_str(),
                    OsStr::new("-N"),
                    OsStr::new(""),
                    OsStr::new("-C"),
                    comment.as_os_str(),
                ]
            );
            fs::write(&private, b"private")?;
            fs::write(ssh.join("id_ed25519.pub"), b"ssh-ed25519 AAAA generated\n")?;
            Ok(ssh_keygen_output(0, b"", b""))
        })?;

        assert_eq!(
            fs::metadata(&ssh)?.permissions().mode() & 0o777,
            SSH_DIRECTORY_MODE
        );
        assert_eq!(
            fs::metadata(ssh.join("id_ed25519"))?.permissions().mode() & 0o777,
            SSH_PRIVATE_KEY_MODE
        );
        assert_eq!(
            fs::metadata(ssh.join("id_ed25519.pub"))?
                .permissions()
                .mode()
                & 0o777,
            SSH_PUBLIC_KEY_MODE
        );
        Ok(())
    }

    #[test]
    // invalid key stateとmismatched key pairを拒否することを検証する
    fn rejects_invalid_ssh_key_state() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        fs::create_dir(&ssh)?;
        fs::write(ssh.join("id_ed25519.pub"), b"ssh-ed25519 AAAA public\n")?;
        assert!(bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| unreachable!()).is_err());

        fs::write(ssh.join("id_ed25519"), b"private")?;
        assert!(
            bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| Ok(ssh_keygen_output(
                0,
                b"ssh-ed25519 BBBB derived\n",
                b""
            )))
            .is_err()
        );
        Ok(())
    }

    #[test]
    // missing USER・symlink・malformed public keyを拒否することを検証する
    fn rejects_invalid_ssh_bootstrap_inputs() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        assert!(bootstrap_ssh_with(&home, None, |_| unreachable!()).is_err());

        let linked_home = directory.path.join("linked-home");
        fs::create_dir(&linked_home)?;
        symlink(home.join(".ssh"), linked_home.join(".ssh"))?;
        assert!(
            bootstrap_ssh_with(&linked_home, Some(OsStr::new("user")), |_| unreachable!()).is_err()
        );

        for content in [
            b"".as_slice(),
            b"ssh-ed25519",
            b"ssh-ed25519 AAAA\nsecond BBBB",
        ] {
            assert!(parse_public_key(content).is_err());
        }
        Ok(())
    }

    #[test]
    // ssh-keygen成功後のinvalid key pairを今回の生成物としてcleanupすることを検証する
    fn cleans_invalid_generated_ssh_key_pair() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        assert!(
            bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| {
                fs::write(ssh.join("id_ed25519"), b"partial private")?;
                fs::write(ssh.join("id_ed25519.pub"), b"partial")?;
                Ok(ssh_keygen_output(0, b"", b""))
            })
            .is_err()
        );
        assert!(!ssh.join("id_ed25519").exists());
        assert!(!ssh.join("id_ed25519.pub").exists());
        Ok(())
    }

    #[test]
    // ssh-keygen失敗時に別processが作成した可能性のあるkeyを削除しない
    fn preserves_unowned_keys_after_ssh_keygen_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        assert!(
            bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| {
                fs::write(ssh.join("id_ed25519"), b"concurrent private")?;
                fs::write(ssh.join("id_ed25519.pub"), b"ssh-ed25519 AAAA concurrent\n")?;
                Ok(ssh_keygen_output(1, b"", b"path appeared"))
            })
            .is_err()
        );
        assert_eq!(fs::read(ssh.join("id_ed25519"))?, b"concurrent private");
        assert_eq!(
            fs::read(ssh.join("id_ed25519.pub"))?,
            b"ssh-ed25519 AAAA concurrent\n"
        );
        Ok(())
    }

    #[test]
    // authorized_keys temporary衝突時に既存temporary fileを削除しない
    fn preserves_colliding_authorized_keys_temporary_file() -> Result<()> {
        let directory = TestDirectory::new()?;
        let authorized = directory.path.join("authorized_keys");
        let temporary = directory.path.join("existing-temporary");
        fs::write(&temporary, b"unowned temporary")?;
        let key = parse_public_key(b"ssh-ed25519 AAAA key")?;

        assert!(
            replace_authorized_keys(&authorized, &temporary, b"", &key, AUTHORIZED_KEYS_MODE)
                .is_err()
        );
        assert_eq!(fs::read(&temporary)?, b"unowned temporary");
        assert!(!authorized.exists());
        Ok(())
    }

    #[test]
    // authorized_keysのbinary内容・newline・既存permissionを維持してkeyを追加する
    fn appends_ssh_authorized_key_atomically() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        fs::create_dir(&ssh)?;
        fs::write(ssh.join("id_ed25519"), b"private")?;
        fs::write(ssh.join("id_ed25519.pub"), b"ssh-ed25519 AAAA public\n")?;
        let authorized_path = ssh.join("authorized_keys");
        fs::write(&authorized_path, b"unrelated-\xff-line")?;
        fs::set_permissions(&authorized_path, fs::Permissions::from_mode(0o640))?;

        bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| {
            Ok(ssh_keygen_output(0, b"ssh-ed25519 AAAA\n", b""))
        })?;

        assert_eq!(
            fs::read(&authorized_path)?,
            b"unrelated-\xff-line\nssh-ed25519 AAAA public\n"
        );
        assert_eq!(
            fs::metadata(&authorized_path)?.permissions().mode() & 0o777,
            0o640
        );
        assert!(fs::read_dir(&ssh)?.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".authorized_keys.eiyah.")
        }));
        Ok(())
    }

    #[test]
    // managed directoryをmode 0755で作成し既存directoryのpermissionは維持する
    fn creates_and_reuses_install_directories() -> Result<()> {
        let directory = TestDirectory::new()?;
        let managed = directory.path.join("missing-parent/managed");
        assert_eq!(
            create_install_directories(&managed)?,
            vec![directory.path.join("missing-parent"), managed.clone()]
        );
        assert_eq!(fs::metadata(&managed)?.permissions().mode() & 0o777, 0o755);

        fs::set_permissions(&managed, fs::Permissions::from_mode(0o750))?;
        assert!(create_install_directories(&managed)?.is_empty());
        assert_eq!(fs::metadata(&managed)?.permissions().mode() & 0o777, 0o750);
        Ok(())
    }

    #[test]
    // managed directory位置のsymlinkとnon-directoryを拒否する
    fn rejects_invalid_install_directory_paths() -> Result<()> {
        let directory = TestDirectory::new()?;
        let regular = directory.path.join("regular");
        fs::write(&regular, b"file")?;
        assert!(create_install_directories(&regular).is_err());

        let actual = directory.path.join("actual");
        let link = directory.path.join("link");
        fs::create_dir(&actual)?;
        symlink(&actual, &link)?;
        assert!(create_install_directories(&link).is_err());
        Ok(())
    }

    #[test]
    // 作成直前に他process相当で出現したdirectoryを所有せずpermissionも維持する
    fn preserves_directory_created_during_install_race() -> Result<()> {
        let directory = TestDirectory::new()?;
        let managed = directory.path.join("managed");

        let created = create_install_directories_with(&managed, |path| {
            fs::create_dir(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            Ok(())
        })?;

        assert!(created.is_empty());
        assert_eq!(fs::metadata(&managed)?.permissions().mode() & 0o777, 0o700);
        Ok(())
    }

    #[test]
    // source binaryがregular executableであることを検証する
    fn validates_eiyah_install_source() -> Result<()> {
        let directory = TestDirectory::new()?;
        let source = directory.path.join("source");
        fs::write(&source, b"binary")?;
        assert!(validate_source_binary(&source).is_err());

        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
        validate_source_binary(&source)?;

        let link = directory.path.join("source-link");
        symlink(&source, &link)?;
        assert!(validate_source_binary(&link).is_err());
        assert!(validate_source_binary(&directory.path).is_err());
        Ok(())
    }

    #[test]
    // Eiyah binaryをsource内容とmode 0755で新規配置し既存targetを拒否する
    fn installs_eiyah_binary_without_replacement() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let binary_directory = paths.eiyah_prefix.join("bin");
        fs::create_dir_all(&binary_directory)?;
        let source = directory.path.join("source-eiyah");
        fs::write(&source, b"running eiyah")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;

        install_eiyah_binary_from(&paths, &source)?;

        let target = binary_directory.join("eiyah");
        assert_eq!(fs::read(&target)?, b"running eiyah");
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o755);
        assert!(install_eiyah_binary_from(&paths, &source).is_err());
        assert_eq!(fs::read(&target)?, b"running eiyah");
        Ok(())
    }

    #[test]
    // binary copy失敗時に今回作成したpartial targetだけをcleanupする
    fn cleans_partial_eiyah_binary_after_copy_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(paths.eiyah_prefix.join("bin"))?;
        let source = directory.path.join("source-eiyah");
        fs::write(&source, b"running eiyah")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;

        assert!(
            install_eiyah_binary_with(&paths, &source, |_, target| {
                target.write_all(b"partial")?;
                Err(io::Error::other("injected copy failure"))
            })
            .is_err()
        );
        assert!(!paths.eiyah_prefix.join("bin/eiyah").exists());
        assert_eq!(fs::read(&source)?, b"running eiyah");
        Ok(())
    }

    #[test]
    // copy失敗前に置換された他者所有targetをcleanupしない
    fn preserves_replaced_eiyah_target_after_copy_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let binary_directory = paths.eiyah_prefix.join("bin");
        fs::create_dir_all(&binary_directory)?;
        let source = directory.path.join("source-eiyah");
        fs::write(&source, b"running eiyah")?;
        fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
        let target = binary_directory.join("eiyah");

        assert!(
            install_eiyah_binary_with(&paths, &source, |_, _| {
                fs::remove_file(&target)?;
                fs::write(&target, b"concurrent target")?;
                Err(io::Error::other("injected copy failure"))
            })
            .is_err()
        );
        assert_eq!(fs::read(&target)?, b"concurrent target");
        Ok(())
    }

    #[test]
    // public entryをinstalled binaryへのabsolute symlinkとして新規作成する
    fn creates_absolute_eiyah_public_entry() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir_all(home.join(".local/bin"))?;

        let public_entry = create_eiyah_public_entry(&paths, &home)?;

        let target = fs::read_link(&public_entry)?;
        assert!(target.is_absolute());
        assert_eq!(target, paths.eiyah_prefix.join("bin/eiyah"));
        Ok(())
    }

    #[test]
    // public entry衝突時に既存pathを変更またはcleanupしない
    fn preserves_existing_eiyah_public_entry() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let public_entry = home.join(".local/bin/eiyah");
        fs::create_dir_all(public_entry.parent().unwrap())?;
        fs::write(&public_entry, b"concurrent entry")?;

        assert!(create_eiyah_public_entry(&paths, &home).is_err());
        assert_eq!(fs::read(&public_entry)?, b"concurrent entry");
        Ok(())
    }

    // commandで明示的に削除されたenvironment名を確認する
    fn environment_is_removed(command: &Command, expected: &str) -> bool {
        command
            .get_envs()
            .any(|(name, value)| name == expected && value.is_none())
    }

    // Pixi bootstrap test用Private manifestを作成する
    fn create_core_manifest(root: &Path, contents: &[u8]) -> Result<PathBuf> {
        let manifest = root.join("pixi/pixi-global.toml");
        fs::create_dir_all(manifest.parent().unwrap())?;
        fs::write(&manifest, contents)?;
        Ok(manifest)
    }

    // test installerとしてexpected Pixi binaryを作成する
    fn create_test_pixi_binary(paths: &ResolvedPaths) -> Result<()> {
        let binary = paths.pixi_home.join("bin/pixi");
        fs::create_dir_all(binary.parent().unwrap())?;
        fs::write(&binary, b"test pixi")?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    #[test]
    // installer・binary validation・manifest配置・global sync contractを接続する
    fn bootstraps_pixi_with_canonical_commands_and_manifest() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        let manifest_contents = b"version = 1\n";
        create_core_manifest(&core_root, manifest_contents)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;

        bootstrap_pixi_with(
            &paths,
            &core_root,
            &home,
            |url| {
                assert_eq!(url, PIXI_INSTALLER_URL);
                Ok(b"#!/usr/bin/bash\n".to_vec())
            },
            |_| Ok(()),
            |command, script| {
                assert_eq!(command.get_program(), BASH_PATH);
                assert_eq!(script, b"#!/usr/bin/bash\n");
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(name, _)| *name == "PIXI_HOME")
                        .unwrap()
                        .1,
                    Some(paths.pixi_home.as_os_str())
                );
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(name, _)| *name == "PIXI_NO_PATH_UPDATE")
                        .unwrap()
                        .1,
                    Some(OsStr::new("1"))
                );
                for name in [
                    "PIXI_BIN_DIR",
                    "PIXI_VERSION",
                    "PIXI_ARCH",
                    "PIXI_DOWNLOAD_URL",
                    "PIXI_CACHE_DIR",
                    "RATTLER_CACHE_DIR",
                    "NETRC",
                    "TMP_DIR",
                ] {
                    assert!(environment_is_removed(command, name));
                }
                create_test_pixi_binary(&paths)?;
                Ok(std::process::ExitStatus::from_raw(0))
            },
            |command| {
                assert_eq!(command.get_program(), paths.pixi_home.join("bin/pixi"));
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    [OsStr::new("--version")]
                );
                Ok(ssh_keygen_output(0, b"pixi 0.50.0\n", b"ignored"))
            },
            |command| {
                assert_eq!(command.get_program(), paths.pixi_home.join("bin/pixi"));
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    [OsStr::new("global"), OsStr::new("sync")]
                );
                assert_eq!(command.get_current_dir(), Some(home.as_path()));
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(name, _)| *name == "PIXI_HOME")
                        .unwrap()
                        .1,
                    Some(paths.pixi_home.as_os_str())
                );
                assert_eq!(
                    command
                        .get_envs()
                        .find(|(name, _)| *name == "PIXI_NO_PATH_UPDATE")
                        .unwrap()
                        .1,
                    Some(OsStr::new("1"))
                );
                for name in ["PIXI_BIN_DIR", "PIXI_CACHE_DIR", "RATTLER_CACHE_DIR"] {
                    assert!(environment_is_removed(command, name));
                }
                Ok(std::process::ExitStatus::from_raw(0))
            },
        )?;

        let manifest = paths.pixi_home.join("manifests/pixi-global.toml");
        assert_eq!(fs::read(&manifest)?, manifest_contents);
        assert_eq!(fs::metadata(&manifest)?.permissions().mode() & 0o777, 0o644);
        assert_eq!(
            fs::metadata(paths.pixi_home.join("manifests"))?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&paths.pixi_home)?.permissions().mode() & 0o777,
            0o755
        );
        Ok(())
    }

    #[test]
    // PIXI_HOME作成raceで他者所有directoryを変更またはcleanupしない
    fn preserves_pixi_home_created_during_race() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        let home = directory.path.join("home");

        assert!(
            bootstrap_pixi_with(
                &paths,
                &core_root,
                &home,
                |_| Ok(b"installer".to_vec()),
                |pixi_home| {
                    fs::create_dir(pixi_home)?;
                    fs::set_permissions(pixi_home, fs::Permissions::from_mode(0o700))?;
                    Ok(())
                },
                |_, _| unreachable!(),
                |_| unreachable!(),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert!(paths.pixi_home.is_dir());
        assert_eq!(
            fs::metadata(&paths.pixi_home)?.permissions().mode() & 0o777,
            0o700
        );
        Ok(())
    }

    #[test]
    // empty installerはhome作成前に拒否しspawn failure後は所有homeをcleanupする
    fn rejects_invalid_pixi_installer_without_leaving_home() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        let home = directory.path.join("home");

        assert!(
            bootstrap_pixi_with(
                &paths,
                &core_root,
                &home,
                |_| Ok(Vec::new()),
                |_| Ok(()),
                |_, _| unreachable!(),
                |_| unreachable!(),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert!(!paths.pixi_home.exists());

        assert!(
            bootstrap_pixi_with(
                &paths,
                &core_root,
                &home,
                |_| Ok(b"installer".to_vec()),
                |_| Ok(()),
                |_, _| Err(anyhow::anyhow!("injected spawn failure")),
                |_| unreachable!(),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert!(!paths.pixi_home.exists());
        Ok(())
    }

    #[test]
    // installer失敗時は所有PIXI_HOMEをcleanupしsync失敗時はrollback用に保持する
    fn applies_pixi_cleanup_boundary_at_global_sync() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        create_core_manifest(&core_root, b"version = 1\n")?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;

        assert!(
            bootstrap_pixi_with(
                &paths,
                &core_root,
                &home,
                |_| Ok(b"installer".to_vec()),
                |_| Ok(()),
                |_, _| Ok(std::process::ExitStatus::from_raw(1 << 8)),
                |_| unreachable!(),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert!(!paths.pixi_home.exists());

        assert!(
            bootstrap_pixi_with(
                &paths,
                &core_root,
                &home,
                |_| Ok(b"installer".to_vec()),
                |_| Ok(()),
                |_, _| {
                    create_test_pixi_binary(&paths)?;
                    Ok(std::process::ExitStatus::from_raw(0))
                },
                |_| Ok(ssh_keygen_output(0, b"pixi 0.50.0\n", b"")),
                |_| Ok(std::process::ExitStatus::from_raw(1 << 8)),
            )
            .is_err()
        );
        assert!(paths.pixi_home.is_dir());
        assert!(paths.pixi_home.join("manifests/pixi-global.toml").is_file());
        Ok(())
    }

    #[test]
    // Pixi binaryとPrivate manifest sourceの不正なfilesystem形状を拒否する
    fn rejects_invalid_pixi_binary_and_manifest_source() -> Result<()> {
        fn unexpected_execution(_: &mut Command) -> Result<Output> {
            unreachable!()
        }

        let directory = TestDirectory::new()?;
        let binary = directory.path.join("pixi");
        fs::write(&binary, b"pixi")?;
        let mut execute = unexpected_execution;
        assert!(validate_pixi_binary_with(&binary, &mut execute).is_err());
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let mut failed = |_: &mut Command| Ok(ssh_keygen_output(1, b"pixi 0.50.0\n", b""));
        assert!(validate_pixi_binary_with(&binary, &mut failed).is_err());
        let mut empty = |_: &mut Command| Ok(ssh_keygen_output(0, b"", b"ignored"));
        assert!(validate_pixi_binary_with(&binary, &mut empty).is_err());
        let link = directory.path.join("pixi-link");
        symlink(&binary, &link)?;
        assert!(validate_pixi_binary_with(&link, &mut execute).is_err());

        let missing = directory.path.join("missing-manifest");
        assert!(validate_pixi_manifest_source(&missing).is_err());
        let manifest_link = directory.path.join("manifest-link");
        symlink(&binary, &manifest_link)?;
        assert!(validate_pixi_manifest_source(&manifest_link).is_err());
        Ok(())
    }

    #[test]
    // manifest衝突とcopy失敗で他者所有pathを変更せずpartial targetだけをcleanupする
    fn preserves_existing_pixi_manifest_and_cleans_partial_copy() -> Result<()> {
        let directory = TestDirectory::new()?;
        let source = directory.path.join("source.toml");
        let target = directory.path.join("target.toml");
        fs::write(&source, b"version = 1\n")?;
        fs::write(&target, b"concurrent manifest")?;
        assert!(place_pixi_manifest(&source, &target).is_err());
        assert_eq!(fs::read(&target)?, b"concurrent manifest");

        fs::remove_file(&target)?;
        assert!(
            place_pixi_manifest_with(&source, &target, |_, target| {
                target.write_all(b"partial")?;
                Err(io::Error::other("injected copy failure"))
            })
            .is_err()
        );
        assert!(!target.exists());
        assert_eq!(fs::read(&source)?, b"version = 1\n");

        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        create_core_manifest(&core_root, b"version = 1\n")?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let raced_manifest = paths.pixi_home.join("manifests/pixi-global.toml");
        assert!(
            bootstrap_pixi_with(
                &paths,
                &core_root,
                &home,
                |_| Ok(b"installer".to_vec()),
                |_| Ok(()),
                |_, _| {
                    create_test_pixi_binary(&paths)?;
                    fs::create_dir(paths.pixi_home.join("manifests"))?;
                    fs::write(&raced_manifest, b"concurrent manifest")?;
                    Ok(std::process::ExitStatus::from_raw(0))
                },
                |_| Ok(ssh_keygen_output(0, b"pixi 0.50.0\n", b"")),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert_eq!(fs::read(&raced_manifest)?, b"concurrent manifest");
        Ok(())
    }
}
