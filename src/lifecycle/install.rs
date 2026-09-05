// ==================================================
// @file src/lifecycle/install.rs
// @brief Initial Eiyah installation
// ==================================================

use std::collections::BTreeSet;
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
#[cfg(test)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Error, Result, bail};
use serde::Deserialize;

use crate::config::{
    ResolvedPaths, create_initial_config, create_install_metadata, discover_install_metadata,
    load_config, load_install_metadata, resolve_paths, runtime_home,
};
use crate::transaction::{
    Action, InitialPublish, LockGuard, PathIdentity, Transaction, append_backup_index_entry,
    encode_backup_index_entry, move_without_replace,
};

use super::update::update_locked;
use super::{
    GITHUB_ACCEPT, GITHUB_API_VERSION, SHA256_HEX_LENGTH, http_agent, require_https_url,
    verify_checksum,
};

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
pub(super) const INSTALL_DIRECTORY_MODE: u32 = 0o755;
// install済みEiyah binary permission
const EIYAH_BINARY_MODE: u32 = 0o755;
// Pixi official installer URL
const PIXI_INSTALLER_URL: &str = "https://pixi.sh/install.sh";
// Pixi installerを実行するsystem Bash
const BASH_PATH: &str = "/usr/bin/bash";
// Pixi global manifest fileのpermission
const PIXI_MANIFEST_MODE: u32 = 0o644;
// backup directoryをuser以外から隠すpermission
const BACKUP_DIRECTORY_MODE: u32 = 0o700;
// generated Git identity fileのpermission
const GIT_CONFIG_LOCAL_MODE: u32 = 0o600;
// show-cad-status download中のpermission
const SHOW_CAD_STATUS_DOWNLOAD_MODE: u32 = 0o600;
// install済みshow-cad-status binaryのpermission
const SHOW_CAD_STATUS_EXECUTABLE_MODE: u32 = 0o755;
// authorized_keys temporary file名の衝突を避けるprocess内連番
static AUTHORIZED_KEYS_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
// Private archive attempt directoryの衝突を避けるprocess内連番
static INSTALL_ATTEMPT_COUNTER: AtomicU64 = AtomicU64::new(0);

// Private archive操作に使用するsystem tar
const TAR_PATH: &str = "/usr/bin/tar";

/// Private installに使用するsame-tag archiveとRelease assetの情報
#[derive(Clone, Debug, Eq, PartialEq)]
struct PrivateReleaseInfo {
    /// latest stable Private Releaseのtag
    tag_name: String,
    /// same tagをrefに使用するrepository archive URL
    archive_url: String,
    /// show-cad-status binary assetのGitHub ID
    show_cad_status_asset_id: u64,
    /// show-cad-status checksum assetのGitHub ID
    show_cad_status_checksum_asset_id: u64,
}

// 利用者へ表示するSSH setup結果
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SshSetupResult {
    ExistingAuthorized,
    CreatedPublic { authorization_added: bool },
    Generated { authorization_added: bool },
    ExistingAuthorizedAdded,
}

impl SshSetupResult {
    // install失敗後にも残るfilesystem変更の有無を返す
    fn changed(self) -> bool {
        !matches!(self, Self::ExistingAuthorized)
    }
}

// --------------------------------------------------
// Eiyah Installation Paths
// --------------------------------------------------

/// 初回installで作成した管理対象directoryだけを作成順で返す
fn create_install_directories(path: &Path) -> Result<Vec<PathBuf>> {
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
fn install_running_eiyah_binary(paths: &ResolvedPaths) -> Result<()> {
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
        .with_context(|| format!("failed to open Eiyah binary: {}", source.display()))?;
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
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("failed to inspect Eiyah binary: {}", source.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("Eiyah binary must be a regular file: {}", source.display());
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        bail!("Eiyah binary must be executable: {}", source.display());
    }
    Ok(())
}

/// HOME配下へinstalled binaryを指すabsolute public symlinkを作成する
fn create_eiyah_public_entry(paths: &ResolvedPaths, home: &Path) -> Result<PathBuf> {
    let target = paths.eiyah_prefix.join("bin/eiyah");
    if !target.is_absolute() {
        bail!(
            "Eiyah command link target must be absolute: {}",
            target.display()
        );
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

/// Transactionへ即時記録する新規managed root
#[derive(Clone, Debug, Eq, PartialEq)]
struct CreatedManagedRoot {
    /// 新規作成したmanaged root path
    path: PathBuf,
    /// 作成直後のfilesystem identity
    identity: PathIdentity,
}

/// Pixi installerとmanifest配置までを行いmanaged rootを返す
fn prepare_pixi(paths: &ResolvedPaths, core_root: &Path) -> Result<CreatedManagedRoot> {
    prepare_pixi_with(
        paths,
        core_root,
        download_pixi_installer,
        |_| Ok(()),
        run_pixi_installer,
        |command| command.output().map_err(Into::into),
        PathIdentity::from_path,
    )
}

/// prepared Pixi environmentをglobal manifestへ同期する
fn sync_pixi(paths: &ResolvedPaths) -> Result<()> {
    let home = runtime_home()?;
    let binary = paths.pixi_home.join("bin/pixi");
    let status = pixi_sync_command(&binary, &paths.pixi_home, &home).status()?;
    if !status.success() {
        bail!("pixi global sync failed: {status}");
    }
    Ok(())
}

// external I/Oを差し替え可能にしてPixi bootstrap contractを実行する
#[cfg(test)]
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
    prepare_pixi_with(
        paths,
        core_root,
        download,
        before_home_create,
        &mut install,
        &mut version,
        PathIdentity::from_path,
    )?;
    let pixi_binary = paths.pixi_home.join("bin/pixi");
    let mut command = pixi_sync_command(&pixi_binary, &paths.pixi_home, home);
    let status = sync(&mut command).context("failed to execute pixi global sync")?;
    if !status.success() {
        bail!("pixi global sync failed: {status}");
    }
    Ok(())
}

// sync開始前のPixi準備を完了しidentity取得失敗もowned root cleanup対象にする
fn prepare_pixi_with<Download, BeforeHome, Installer, Version, Identity>(
    paths: &ResolvedPaths,
    core_root: &Path,
    download: Download,
    before_home_create: BeforeHome,
    mut install: Installer,
    mut version: Version,
    identity: Identity,
) -> Result<CreatedManagedRoot>
where
    Download: FnOnce(&str) -> Result<Vec<u8>>,
    BeforeHome: FnOnce(&Path) -> Result<()>,
    Installer: FnMut(&mut Command, &[u8]) -> Result<ExitStatus>,
    Version: FnMut(&mut Command) -> Result<Output>,
    Identity: FnOnce(&Path) -> Result<PathIdentity>,
{
    let installer_script = download(PIXI_INSTALLER_URL)?;
    if installer_script.is_empty() {
        bail!("Pixi installer script is empty");
    }

    before_home_create(&paths.pixi_home)?;
    let pixi_home_metadata = create_pixi_home(&paths.pixi_home)?;
    let mut unowned_manifest_present = false;
    let result = (|| -> Result<CreatedManagedRoot> {
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

        Ok(CreatedManagedRoot {
            path: paths.pixi_home.clone(),
            identity: identity(&paths.pixi_home)?,
        })
    })();

    if result.is_err() && !unowned_manifest_present {
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

// --------------------------------------------------
// Private Environment Installation
// --------------------------------------------------

/// backupとindex更新成功後に`BackedUp`へ変換するrecord
#[derive(Clone, Debug, Eq, PartialEq)]
struct BackupMove {
    /// HOME配下の元path
    from: PathBuf,
    /// HOME相対layoutを保持したbackup path
    to: PathBuf,
    /// backup index path
    index: PathBuf,
    /// lowercase hexでencodeしたindex entry
    entry: Vec<u8>,
    /// initial index publish後のbest-effort temporary cleanup error
    cleanup_error: Option<String>,
}

/// 展開済みPrivate rootとdotfiles sourceのfilesystem形状を検証する
fn validate_private_source(core_root: &Path) -> Result<PathBuf> {
    validate_non_symlink_directory(core_root, "configuration")?;
    let dotfiles = core_root.join("dotfiles");
    validate_non_symlink_directory(&dotfiles, "dotfiles")?;
    Ok(dotfiles)
}

// symlinkを追跡せずexisting directoryであることを保証する
pub(super) fn validate_non_symlink_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("{label} is unavailable: {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "{label} must be a non-symlink directory: {}",
            path.display()
        );
    }
    Ok(())
}

/// HOME配下のexisting pathをHOME相対layoutのbackupへatomic no-replaceで移動する
fn backup_home_path(home: &Path, state_home: &Path, source: &Path) -> Result<Option<BackupMove>> {
    backup_home_path_with(home, state_home, source, append_backup_index_entry)
}

// index更新failureをtest可能にしてHOME pathをbackupする
fn backup_home_path_with(
    home: &Path,
    state_home: &Path,
    source: &Path,
    update_index: impl FnOnce(&Path, &[u8]) -> Result<InitialPublish>,
) -> Result<Option<BackupMove>> {
    let relative = source
        .strip_prefix(home)
        .with_context(|| format!("backup source must be under HOME: {}", source.display()))?;
    if relative.as_os_str().is_empty()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!(
            "backup source has an invalid HOME-relative path: {}",
            source.display()
        );
    }
    if !path_exists(source)? {
        return Ok(None);
    }

    let target = state_home.join("eiyah/backup/home").join(relative);
    let index = state_home.join("eiyah/backup/index");
    let entry = encode_backup_index_entry(relative)?;
    let parent = target
        .parent()
        .with_context(|| format!("backup target has no parent: {}", target.display()))?;
    create_backup_directories(state_home, parent)?;
    move_without_replace(source, &target).with_context(|| {
        format!(
            "failed to backup {} to {}",
            source.display(),
            target.display()
        )
    })?;
    let published = match update_index(&index, &entry) {
        Ok(published) => published,
        Err(index_error) => {
            return match move_without_replace(&target, source) {
                Ok(()) => Err(index_error),
                Err(restore_error) => Err(anyhow::anyhow!(
                    "{index_error:#}; failed to restore unindexed backup: {restore_error:#}"
                )),
            };
        }
    };
    Ok(Some(BackupMove {
        from: source.to_path_buf(),
        to: target,
        index,
        entry,
        cleanup_error: published.cleanup_error,
    }))
}

// 成功済みbackupをfallibleなuser-facing出力より先にTransactionへ記録する
fn record_backup(transaction: &mut Transaction, moved: BackupMove) -> Result<()> {
    record_backup_with(transaction, moved, crate::ui::print_detail)
}

// detail出力failureをtest可能にして成功済みbackupを記録・表示する
fn record_backup_with(
    transaction: &mut Transaction,
    moved: BackupMove,
    print_detail: impl FnOnce(&str) -> io::Result<()>,
) -> Result<()> {
    let BackupMove {
        from,
        to,
        index,
        entry,
        cleanup_error,
    } = moved;
    let detail = format!("Backed up: {}", from.display());
    transaction.record(Action::BackedUp {
        from,
        to,
        index,
        entry,
    });
    print_detail(&detail)?;
    if let Some(error) = cleanup_error {
        crate::ui::print_warning(&format!("failed to remove temporary files: {error}"));
    }
    Ok(())
}

// Private environment処理に先立ち固定backup rootを作成または検証する
fn prepare_backup_root(state_home: &Path) -> Result<()> {
    create_backup_directories(state_home, &state_home.join("eiyah/backup/home"))
}

// backup parentをmode 0700で作成しexisting directoryのpermissionを維持する
fn create_backup_directories(state_home: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(state_home).with_context(|| {
        format!(
            "backup directory must be under state home: {}",
            path.display()
        )
    })?;
    let mut directory = state_home.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("backup directory has an invalid path: {}", path.display());
        };
        directory.push(component);
        let mut builder = DirBuilder::new();
        builder.mode(BACKUP_DIRECTORY_MODE);
        match builder.create(&directory) {
            Ok(()) => fs::set_permissions(
                &directory,
                fs::Permissions::from_mode(BACKUP_DIRECTORY_MODE),
            )?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_non_symlink_directory(&directory, "backup directory")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Private dotfiles treeを新規`$HOME/.dotfiles`へfile typeを保持してcopyする
fn install_dotfiles(core_root: &Path, home: &Path) -> Result<PathBuf> {
    install_dotfiles_with(core_root, home, |_| Ok(()))
}

// entry作成直前のraceをtest可能にして所有pathだけをcleanupする
fn install_dotfiles_with(
    core_root: &Path,
    home: &Path,
    mut before_create: impl FnMut(&Path) -> Result<()>,
) -> Result<PathBuf> {
    let source = validate_private_source(core_root)?;
    let target = home.join(".dotfiles");
    let metadata = create_owned_directory(&target, INSTALL_DIRECTORY_MODE)?;
    let mut created = vec![(target.clone(), metadata)];
    let result = copy_directory_contents(&source, &target, &mut created, &mut before_create);
    if result.is_err() {
        cleanup_owned_entries(&created);
    }
    result.map(|_| target)
}

// 新規directoryを排他的に作成してidentityを返す
fn create_owned_directory(path: &Path, mode: u32) -> Result<fs::Metadata> {
    let mut builder = DirBuilder::new();
    builder.mode(mode);
    builder.create(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
        let _ = remove_directory_if_same_inode(path, &metadata);
        return Err(error.into());
    }
    Ok(metadata)
}

// directory treeを既存targetを上書きせずrecursive copyする
fn copy_directory_contents(
    source: &Path,
    target: &Path,
    created: &mut Vec<(PathBuf, fs::Metadata)>,
    before_create: &mut impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        before_create(&target_path)?;
        if metadata.file_type().is_dir() {
            let mut builder = DirBuilder::new();
            builder.mode(metadata.permissions().mode() & 0o7777);
            builder.create(&target_path)?;
            created.push((target_path.clone(), fs::symlink_metadata(&target_path)?));
            fs::set_permissions(&target_path, metadata.permissions())?;
            copy_directory_contents(&source_path, &target_path, created, before_create)?;
        } else if metadata.file_type().is_file() {
            let mut source_file = File::open(&source_path)?;
            let mut target_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(metadata.permissions().mode() & 0o7777)
                .open(&target_path)?;
            created.push((target_path.clone(), target_file.metadata()?));
            io::copy(&mut source_file, &mut target_file)?;
            target_file.set_permissions(metadata.permissions())?;
            target_file.sync_all()?;
        } else if metadata.file_type().is_symlink() {
            symlink(fs::read_link(&source_path)?, &target_path)?;
            created.push((target_path.clone(), fs::symlink_metadata(&target_path)?));
        } else {
            bail!(
                "unsupported dotfiles source type: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

// 作成時と同じinodeだけを子から親の順でcleanupする
fn cleanup_owned_entries(created: &[(PathBuf, fs::Metadata)]) {
    for (path, metadata) in created.iter().rev() {
        if metadata.file_type().is_dir() {
            let _ = remove_directory_if_same_inode(path, metadata);
        } else {
            let _ = remove_file_if_same_inode(path, metadata);
        }
    }
}

/// global Git identityからrepository外生成対象の`config.local`を作成する
fn create_git_config_local(dotfiles: &Path) -> Result<PathBuf> {
    create_git_config_local_with(dotfiles, |key| {
        Command::new("git")
            .arg("config")
            .arg("--global")
            .arg(key)
            .output()
            .map_err(Into::into)
    })
}

// Git identity取得を差し替え可能にしてconfig.localをsecure作成する
fn create_git_config_local_with(
    dotfiles: &Path,
    mut execute: impl FnMut(&str) -> Result<Output>,
) -> Result<PathBuf> {
    let name = git_config_value("user.name", &mut execute)?;
    let email = git_config_value("user.email", &mut execute)?;
    let target = dotfiles.join("git/.config/git/config.local");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(GIT_CONFIG_LOCAL_MODE)
        .open(&target)?;
    let created = file.metadata()?;
    let result = (|| -> Result<()> {
        file.set_permissions(fs::Permissions::from_mode(GIT_CONFIG_LOCAL_MODE))?;
        write!(file, "[user]\n    name = {name}\n    email = {email}\n")?;
        file.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        drop(file);
        let _ = remove_file_if_same_inode(&target, &created);
    }
    result.map(|_| target)
}

// Git config stdout末尾のnewlineだけを除去してnon-empty valueを返す
fn git_config_value(key: &str, execute: &mut impl FnMut(&str) -> Result<Output>) -> Result<String> {
    let output = execute(key)?;
    if !output.status.success() {
        bail!("git config --global {key} failed");
    }
    let value = std::str::from_utf8(&output.stdout)?.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        bail!("git config --global {key} is empty");
    }
    Ok(value.to_owned())
}

/// expected Stow executableを検証してsorted package名を返す
pub(super) fn stow_packages(paths: &ResolvedPaths, dotfiles: &Path) -> Result<Vec<OsString>> {
    validate_expected_executable(&paths.pixi_home.join("bin/stow"), "Stow")?;
    let mut packages = Vec::new();
    for entry in fs::read_dir(dotfiles)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
            packages.push(entry.file_name());
        }
    }
    packages.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if packages.is_empty() {
        bail!("dotfiles contains no Stow packages");
    }
    Ok(packages)
}

// expected absolute executableだけを許可する
pub(super) fn validate_expected_executable(path: &Path, label: &str) -> Result<()> {
    if !path.is_absolute() {
        bail!("{label} path must be absolute: {}", path.display());
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.permissions().mode() & 0o111 == 0
    {
        bail!("{label} must be a regular executable: {}", path.display());
    }
    Ok(())
}

/// Stow source treeに対応するHOME conflict targetを重複なしで返す
fn stow_conflicts(dotfiles: &Path, home: &Path, packages: &[OsString]) -> Result<Vec<PathBuf>> {
    let mut conflicts = BTreeSet::new();
    for package in packages {
        collect_stow_conflicts(
            &dotfiles.join(package),
            home,
            dotfiles,
            home,
            &mut conflicts,
        )?;
    }
    Ok(conflicts.into_iter().collect())
}

// source entryをHOME targetへ写像してbackup対象を収集する
fn collect_stow_conflicts(
    source: &Path,
    target: &Path,
    dotfiles: &Path,
    home: &Path,
    conflicts: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        if !target_path.starts_with(home) {
            bail!("Stow target escapes HOME: {}", target_path.display());
        }
        let source_metadata = fs::symlink_metadata(&source_path)?;
        let target_metadata = fs::symlink_metadata(&target_path);
        if source_metadata.file_type().is_dir() {
            match target_metadata {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    collect_stow_conflicts(&source_path, &target_path, dotfiles, home, conflicts)?
                }
                Ok(_) => {
                    conflicts.insert(target_path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else if source_metadata.file_type().is_file() || source_metadata.file_type().is_symlink()
        {
            match target_metadata {
                Ok(metadata)
                    if metadata.file_type().is_symlink()
                        && is_correct_stow_symlink(&target_path, &source_path)? => {}
                Ok(_) => {
                    conflicts.insert(target_path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            bail!("unsupported Stow source type: {}", source_path.display());
        }
    }
    let _ = dotfiles;
    Ok(())
}

// symlink targetをparent基準でlexical解決して対応sourceと比較する
fn is_correct_stow_symlink(target: &Path, source: &Path) -> Result<bool> {
    let link = fs::read_link(target)?;
    let resolved = if link.is_absolute() {
        lexical_normalize(&link)
    } else {
        lexical_normalize(&target.parent().unwrap_or(Path::new("/")).join(link))
    };
    Ok(resolved == lexical_normalize(source))
}

// filesystem accessなしで`.`と`..`を処理する
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// 1 packageをStowし、失敗時は同packageをbest-effortでunstowする
fn run_stow_package(paths: &ResolvedPaths, home: &Path, package: &OsStr) -> Result<()> {
    run_stow_package_with(
        paths,
        home,
        package,
        |command| command.status().map_err(Into::into),
        |command| command.status().map_err(Into::into),
    )
}

// failed package cleanupを差し替え可能にしてoriginal Stow errorをprimaryに保つ
fn run_stow_package_with(
    paths: &ResolvedPaths,
    home: &Path,
    package: &OsStr,
    execute: impl FnMut(&mut Command) -> Result<ExitStatus>,
    cleanup: impl FnOnce(&mut Command) -> Result<ExitStatus>,
) -> Result<()> {
    let package_list = [package.to_os_string()];
    if let Err(error) = run_stow_with(paths, home, &package_list, execute) {
        let executable = paths.pixi_home.join("bin/stow");
        let dotfiles = home.join(".dotfiles");
        let mut command = Command::new(&executable);
        command
            .arg("--delete")
            .arg("--target")
            .arg(home)
            .arg("--dir")
            .arg(&dotfiles)
            .arg(package)
            .current_dir(&dotfiles)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        return match cleanup(&mut command) {
            Ok(status) if status.success() => Err(error),
            Ok(status) => Err(anyhow::anyhow!("{error:#}; failed Stow cleanup: {status}")),
            Err(cleanup_error) => Err(anyhow::anyhow!(
                "{error:#}; failed Stow cleanup: {cleanup_error}"
            )),
        };
    }
    Ok(())
}

// Stow実行を差し替え可能にしてargv・cwd contractを構成する
fn run_stow_with(
    paths: &ResolvedPaths,
    home: &Path,
    packages: &[OsString],
    mut execute: impl FnMut(&mut Command) -> Result<ExitStatus>,
) -> Result<()> {
    let executable = paths.pixi_home.join("bin/stow");
    validate_expected_executable(&executable, "Stow")?;
    let dotfiles = home.join(".dotfiles");
    let mut command = Command::new(executable);
    command
        .arg("--target")
        .arg(home)
        .arg("--dir")
        .arg(&dotfiles)
        .args(packages)
        .current_dir(&dotfiles)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = execute(&mut command)?;
    if !status.success() {
        bail!("Stow failed: {status}");
    }
    Ok(())
}

/// authenticated Private Release assetからshow-cad-status binaryを配置する
fn install_show_cad_status(
    paths: &ResolvedPaths,
    access_token: &str,
    binary_asset_id: u64,
    checksum_asset_id: u64,
) -> Result<PathBuf> {
    install_show_cad_status_with(
        paths,
        binary_asset_id,
        checksum_asset_id,
        |id, target| download_private_asset_to(access_token, id, target),
        |id| download_private_asset(access_token, id),
    )
}

// asset取得を差し替え可能にしてchecksum検証後のbinaryを作成する
fn install_show_cad_status_with(
    paths: &ResolvedPaths,
    binary_asset_id: u64,
    checksum_asset_id: u64,
    mut download_binary: impl FnMut(u64, &mut File) -> Result<()>,
    mut download_checksum: impl FnMut(u64) -> Result<Vec<u8>>,
) -> Result<PathBuf> {
    let target = paths.eiyah_prefix.join("bin/show-cad-status");
    let checksum = parse_show_cad_status_checksum(&download_checksum(checksum_asset_id)?)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(SHOW_CAD_STATUS_DOWNLOAD_MODE)
        .open(&target)?;
    let created = file.metadata()?;
    let result = (|| -> Result<()> {
        file.set_permissions(fs::Permissions::from_mode(SHOW_CAD_STATUS_DOWNLOAD_MODE))?;
        download_binary(binary_asset_id, &mut file)?;
        file.sync_all()?;
        verify_checksum(&target, &checksum).context("show-cad-status checksum does not match")?;
        file.set_permissions(fs::Permissions::from_mode(SHOW_CAD_STATUS_EXECUTABLE_MODE))?;
        file.sync_all()?;
        if file.metadata()?.permissions().mode() & 0o111 == 0 {
            bail!("show-cad-status binary is not executable");
        }
        Ok(())
    })();
    if result.is_err() {
        drop(file);
        let _ = remove_file_if_same_inode(&target, &created);
    }
    result.map(|_| target)
}

// authenticated GitHub asset endpointをoctet-streamとして取得する
fn download_private_asset(access_token: &str, asset_id: u64) -> Result<Vec<u8>> {
    let mut content = Vec::new();
    download_private_asset_to(access_token, asset_id, &mut content)?;
    Ok(content)
}

// authenticated GitHub assetを指定writerへstreamする
fn download_private_asset_to(
    access_token: &str,
    asset_id: u64,
    target: &mut impl Write,
) -> Result<()> {
    let url = private_asset_url(asset_id);
    let agent = http_agent();
    let mut response = agent
        .get(&url)
        .header("Accept", "application/octet-stream")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
        .call()?;
    io::copy(&mut response.body_mut().as_reader(), target)?;
    Ok(())
}

// asset IDからPrivate Release download endpointを構成する
fn private_asset_url(asset_id: u64) -> String {
    format!("https://api.github.com/repos/{PRIVATE_REPOSITORY}/releases/assets/{asset_id}")
}

// show-cad-status checksum assetのexact formatをdecodeする
fn parse_show_cad_status_checksum(content: &[u8]) -> Result<[u8; 32]> {
    let text = std::str::from_utf8(content)?;
    let suffix = format!("  {SHOW_CAD_STATUS_ASSET_NAME}\n");
    if text.len() != SHA256_HEX_LENGTH + suffix.len() || !text.ends_with(&suffix) {
        bail!("invalid show-cad-status checksum format");
    }
    let hexadecimal = &text[..SHA256_HEX_LENGTH];
    if !hexadecimal
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("show-cad-status checksum must be lowercase hexadecimal");
    }
    let mut checksum = [0_u8; 32];
    for (index, byte) in checksum.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hexadecimal[index * 2..index * 2 + 2], 16)?;
    }
    Ok(checksum)
}

/// show-cad-status binaryへのabsolute public symlinkを作成する
fn create_show_cad_status_entry(paths: &ResolvedPaths, home: &Path) -> Result<PathBuf> {
    let target = paths.eiyah_prefix.join("bin/show-cad-status");
    if !target.is_absolute() {
        bail!("show-cad-status entry target must be absolute");
    }
    let entry = home.join(".local/bin/show-cad-status");
    symlink(&target, &entry)?;
    Ok(entry)
}

/// Stow後の`.cshrc`がdotfiles sourceを指すsymlinkであることを検証する
fn validate_stowed_cshrc(dotfiles: &Path, home: &Path) -> Result<()> {
    let source = dotfiles.join("tcsh/.cshrc");
    let target = home.join(".cshrc");
    if !fs::symlink_metadata(&target)?.file_type().is_symlink()
        || !is_correct_stow_symlink(&target, &source)?
    {
        bail!(".cshrc is not linked to the installed dotfiles source");
    }
    Ok(())
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

/// Public Eiyahのinstall状態
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InstallState {
    /// managed artifactがすべて存在しない状態
    NotInstalled,
    /// 必須artifactとmetadataがすべて整合する状態
    Installed,
    /// artifactの不足または構造不整合がある状態
    Partial,
}

/// 展開済みPrivate archive rootとownership情報
struct PrivateArchive {
    // attempt directory
    root: PathBuf,
    // 後続helperへ渡す展開済みeiyah-core root
    core_root: PathBuf,
    // cleanup対象を限定するattempt root identity
    metadata: fs::Metadata,
}

impl PrivateArchive {
    // ownership確認付きでattempt rootをcleanupする
    fn cleanup(&self) -> Result<()> {
        remove_owned_tree(&self.root, &self.metadata).map_err(Into::into)
    }
}

impl Drop for PrivateArchive {
    // early returnでもattempt rootをbest-effort cleanupする
    fn drop(&mut self) {
        let _ = remove_owned_tree(&self.root, &self.metadata);
    }
}

/// install開始前のhost・command・writeability条件を検証する
fn install_preflight(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    if !home.is_absolute() || home.as_os_str().is_empty() {
        bail!("HOME must be an absolute non-empty path");
    }
    let os_release = fs::read_to_string("/etc/os-release")?;
    let id = os_release_value_from(&os_release, "ID");
    let version = os_release_value_from(&os_release, "VERSION_ID");
    if id.as_deref() != Some("almalinux")
        || !version
            .as_deref()
            .is_some_and(|value| value.starts_with("8."))
    {
        bail!("Eiyah requires AlmaLinux 8.x");
    }
    if env::consts::ARCH != "x86_64" {
        bail!("Eiyah requires x86_64");
    }
    let glibc = Command::new("getconf").arg("GNU_LIBC_VERSION").output()?;
    let glibc_text = String::from_utf8(glibc.stdout)?;
    let version = glibc_text.split_whitespace().last().unwrap_or("");
    if !glibc.status.success() || !version_at_least(version, 2, 28) {
        bail!("Eiyah requires glibc >= 2.28");
    }
    for (program, argument) in [("curl", "--version"), ("ssh", "-V"), ("ssh-keygen", "-V")] {
        if Command::new(program)
            .arg(argument)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            bail!("required command is unavailable: {program}");
        }
    }
    for program in [BASH_PATH, TAR_PATH] {
        validate_expected_executable(Path::new(program), "required command")?;
    }
    for path in [
        home,
        &paths.config_home,
        &paths.data_home,
        &paths.state_home,
    ] {
        validate_writable_directory(path)?;
    }
    Ok(())
}

// os-releaseのquoted/unquoted valueを取得する
fn os_release_value_from(content: &str, name: &str) -> Option<String> {
    let value = content
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}=")))?;
    Some(value.trim_matches(['\'', '"']).to_owned())
}

// major.minor versionがminimum以上か判定する
fn version_at_least(value: &str, major: u64, minor: u64) -> bool {
    let mut fields = value.split('.');
    let Some(actual_major) = fields.next().and_then(|field| field.parse().ok()) else {
        return false;
    };
    let Some(actual_minor) = fields.next().and_then(|field| field.parse().ok()) else {
        return false;
    };
    (actual_major, actual_minor) >= (major, minor)
}

// missing pathではnearest existing ancestorのwrite・search permissionを確認する
fn validate_writable_directory(path: &Path) -> Result<()> {
    let mut current = path;
    loop {
        match fs::symlink_metadata(current) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    bail!(
                        "writable path must be a non-symlink directory: {}",
                        current.display()
                    );
                }
                let path = CString::new(current.as_os_str().as_bytes())?;
                // SAFETY: pathはNUL終端されcall中有効
                if unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } != 0 {
                    return Err(io::Error::last_os_error().into());
                }
                return Ok(());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current = current
                    .parent()
                    .context("writable path has no existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

// authenticated same-tag archiveをtemporary rootへstreamして安全に展開する
fn download_private_archive(
    paths: &ResolvedPaths,
    token: &str,
    url: &str,
) -> Result<PrivateArchive> {
    let parent = paths.state_home.join("eiyah/tmp");
    create_install_directories(&parent)?;
    let (root, metadata) = loop {
        let sequence = INSTALL_ATTEMPT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = parent.join(format!("install-{}-{sequence}", std::process::id()));
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&root) {
            Ok(()) => {
                let metadata = match fs::symlink_metadata(&root) {
                    Ok(metadata) => metadata,
                    Err(error) => {
                        let _ = fs::remove_dir(&root);
                        return Err(error.into());
                    }
                };
                if let Err(error) = fs::set_permissions(&root, fs::Permissions::from_mode(0o700)) {
                    let _ = remove_owned_tree(&root, &metadata);
                    return Err(error.into());
                }
                break (root, metadata);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    };
    let archive = PrivateArchive {
        core_root: root.join("core"),
        root: root.clone(),
        metadata,
    };
    let result = (|| -> Result<()> {
        let archive_path = root.join("eiyah-core.tar.gz");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&archive_path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let agent = http_agent();
        let mut response = private_request(&agent, url, token).call()?;
        io::copy(&mut response.body_mut().as_reader(), &mut file)?;
        file.sync_all()?;
        crate::ui::print_operation("Extracting configuration")?;
        extract_private_archive(&archive_path, &archive.core_root)
    })();
    result.map(|_| archive)
}

// inspected archiveをempty core directoryへ安全なtar optionで展開する
fn extract_private_archive(archive: &Path, core_root: &Path) -> Result<()> {
    inspect_archive(archive)?;
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder.create(core_root)?;
    fs::set_permissions(core_root, fs::Permissions::from_mode(0o700))?;
    let status = Command::new(TAR_PATH)
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(core_root)
        .arg("--strip-components=1")
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;
    if !status.success() {
        bail!("configuration extraction failed: {status}");
    }
    validate_non_symlink_directory(core_root, "extracted configuration")
}

// tar listingのentry type・single top-level・path traversalを検証する
fn inspect_archive(path: &Path) -> Result<()> {
    let output = Command::new(TAR_PATH)
        .arg("-tvzf")
        .arg(path)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!("configuration archive inspection failed: {}", output.status);
    }
    let listing = std::str::from_utf8(&output.stdout)?;
    let mut top_level: Option<OsString> = None;
    for line in listing.lines() {
        let kind = line
            .as_bytes()
            .first()
            .copied()
            .context("empty archive listing entry")?;
        if kind != b'-' && kind != b'd' {
            bail!("unsupported configuration archive entry");
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 6 {
            bail!("invalid configuration archive listing entry");
        }
        validate_archive_entry(Path::new(&fields[5..].join(" ")), &mut top_level)?;
    }
    if top_level.is_none() {
        bail!("configuration archive is empty");
    }
    Ok(())
}

// archive entryが単一top-level配下から脱出しないことを確認する
fn validate_archive_entry(path: &Path, top_level: &mut Option<OsString>) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        bail!("invalid configuration archive path");
    }
    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        bail!("invalid configuration archive path");
    };
    if components.any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!("configuration archive path escapes its top-level directory");
    }
    match top_level {
        Some(expected) if expected != first => {
            bail!("configuration archive has multiple top-level directories")
        }
        None => *top_level = Some(first.to_os_string()),
        _ => {}
    }
    Ok(())
}

/// Current Branchのinitial installまたはinstalled updateを実行する
pub(crate) fn run_install() -> Result<()> {
    let home = runtime_home()?;
    let paths = resolve_install_paths(&home)?;
    install_preflight(&paths, &home)?;
    let public_entry = home.join(".local/bin/eiyah");
    let initial_state = detect_install_state(&paths, &public_entry)?;
    route_install_state(
        initial_state,
        || LockGuard::acquire(&paths.state_home),
        || detect_install_state(&paths, &public_entry),
        || update_locked(&paths, true),
        || install_not_installed_flow(&paths, &home),
    )
}

// existing installではpublic entry由来metadataを優先して配置pathを復元する
fn resolve_install_paths(home: &Path) -> Result<ResolvedPaths> {
    resolve_install_paths_with(home, resolve_paths)
}

// initial path解決を差し替え可能にしてinstalled operationのmetadata優先を保証する
fn resolve_install_paths_with(
    home: &Path,
    resolve_initial: impl FnOnce() -> Result<ResolvedPaths>,
) -> Result<ResolvedPaths> {
    let public_entry = home.join(".local/bin/eiyah");
    if !path_exists(&public_entry)? {
        return resolve_initial();
    }

    let metadata_path = discover_install_metadata(&public_entry).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "existing Eiyah installation is incomplete: Eiyah command link is missing or invalid: {}",
                public_entry.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })?;
    let metadata = load_install_metadata(&metadata_path).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "existing Eiyah installation is incomplete: installation information is missing or invalid: {}",
                metadata_path.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })?;
    ResolvedPaths::from_install_metadata(metadata).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "existing Eiyah installation is incomplete: installation paths are invalid: {}",
                metadata_path.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })
}

// install状態をlock境界の契約どおりupdateまたはinitial installへ振り分ける
fn route_install_state<Lock>(
    initial_state: InstallState,
    acquire_lock: impl FnOnce() -> Result<Lock>,
    detect_locked_state: impl FnOnce() -> Result<InstallState>,
    update: impl FnOnce() -> Result<()>,
    install: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if initial_state == InstallState::Partial {
        return Err(crate::ui::UserFacingError::new(
            "existing Eiyah installation is incomplete: required files are missing or invalid",
            Vec::new(),
            Vec::new(),
        )
        .into());
    }
    let _lock = acquire_lock()?;
    if initial_state == InstallState::Installed {
        crate::ui::print_operation("Eiyah is already installed")?;
        return update();
    }
    match detect_locked_state()? {
        InstallState::Installed => {
            crate::ui::print_operation("Eiyah is already installed")?;
            update()
        }
        InstallState::Partial => Err(crate::ui::UserFacingError::new(
            "existing Eiyah installation is incomplete: required files are missing or invalid",
            Vec::new(),
            Vec::new(),
        )
        .into()),
        InstallState::NotInstalled => install(),
    }
}

// authenticated Private artifactを取得してtransaction境界内のinitial installを完了する
fn install_not_installed_flow(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    crate::ui::print_operation("Authorizing with GitHub")?;
    let token = authorize_private_repository()?;
    let release = fetch_private_release(&token)?;
    crate::ui::print_operation("Downloading configuration")?;
    crate::ui::print_detail(&release.tag_name)?;
    crate::ui::print_detail(&release.archive_url)?;
    let archive = download_private_archive(&paths, &token, &release.archive_url)?;
    crate::ui::print_operation("Setting up SSH")?;
    let ssh = bootstrap_ssh(home)?;
    print_ssh_setup(home, ssh)?;
    let mut transaction = Transaction::new();
    let result = complete_install_transaction(
        &mut transaction,
        |transaction| {
            install_not_installed(
                paths,
                home,
                &archive.core_root,
                &token,
                &release,
                transaction,
            )
        },
        || archive.cleanup(),
        crate::ui::print_warning,
    );
    add_ssh_residual_warning(result, ssh)
}

// SSH変更後のinstall failureへ非transaction stateを明示する
fn add_ssh_residual_warning(result: Result<()>, ssh: SshSetupResult) -> Result<()> {
    match result {
        Err(error) if ssh.changed() => Err(crate::ui::UserFacingError::with_warning(
            error,
            "SSH changes made during setup were not reverted.",
        )
        .into()),
        result => result,
    }
}

// install結果に応じてcommitまたはrollbackしarchive cleanupを最後に実行する
fn complete_install_transaction(
    transaction: &mut Transaction,
    install: impl FnOnce(&mut Transaction) -> Result<()>,
    cleanup: impl FnOnce() -> Result<()>,
    mut warn: impl FnMut(&str),
) -> Result<()> {
    let result = install(transaction);
    match result {
        Ok(()) => {
            transaction.commit();
            if let Err(error) = cleanup() {
                warn(&format!("failed to remove temporary files: {error:#}"));
            }
            Ok(())
        }
        Err(error) => {
            let rollback = transaction.rollback();
            let cleanup = cleanup();
            let mut warnings = Vec::new();
            if rollback.is_err() {
                warnings
                    .push("Eiyah could not fully restore the previous system state.".to_owned());
            }
            if let Err(cleanup) = cleanup {
                warnings.push(format!("failed to remove temporary files: {cleanup:#}"));
            }
            Err(crate::ui::UserFacingError::new(format!("{error:#}"), warnings, Vec::new()).into())
        }
    }
}

// lock取得済みNotInstalled stateへ全managed artifactを順序通り配置する
fn install_not_installed(
    paths: &ResolvedPaths,
    home: &Path,
    core_root: &Path,
    token: &str,
    release: &PrivateReleaseInfo,
    transaction: &mut Transaction,
) -> Result<()> {
    install_not_installed_with(
        paths,
        home,
        core_root,
        token,
        release,
        transaction,
        prepare_pixi,
        sync_pixi,
        create_git_config_local,
        run_stow_package,
        install_show_cad_status,
    )
}

// external install boundaryを差し替え可能にしてinitial installを実行する
fn install_not_installed_with<PreparePixi, SyncPixi, CreateGit, RunStow, InstallStatus>(
    paths: &ResolvedPaths,
    home: &Path,
    core_root: &Path,
    token: &str,
    release: &PrivateReleaseInfo,
    transaction: &mut Transaction,
    prepare_pixi: PreparePixi,
    sync_pixi: SyncPixi,
    create_git_config: CreateGit,
    mut run_stow: RunStow,
    install_status: InstallStatus,
) -> Result<()>
where
    PreparePixi: FnOnce(&ResolvedPaths, &Path) -> Result<CreatedManagedRoot>,
    SyncPixi: FnOnce(&ResolvedPaths) -> Result<()>,
    CreateGit: FnOnce(&Path) -> Result<PathBuf>,
    RunStow: FnMut(&ResolvedPaths, &Path, &OsStr) -> Result<()>,
    InstallStatus: FnOnce(&ResolvedPaths, &str, u64, u64) -> Result<PathBuf>,
{
    for directory in [
        paths.eiyah_prefix.clone(),
        paths.eiyah_prefix.join("bin"),
        home.join(".local/bin"),
    ] {
        for created in create_install_directories(&directory)? {
            record_created(transaction, created, false)?;
        }
    }
    crate::ui::print_operation("Installing Eiyah")?;
    crate::ui::print_detail(&home.join(".local/bin/eiyah").display().to_string())?;
    install_running_eiyah_binary(paths)?;
    record_created(transaction, paths.eiyah_prefix.join("bin/eiyah"), false)?;
    let entry = create_eiyah_public_entry(paths, home)?;
    record_created(transaction, entry, false)?;
    let metadata_publish = create_install_metadata(paths)?;
    record_created(transaction, paths.eiyah_prefix.join("install.toml"), false)?;
    if let Some(error) = metadata_publish.cleanup_error {
        crate::ui::print_warning(&format!("failed to remove temporary files: {error}"));
    }

    crate::ui::print_operation("Installing Pixi")?;
    crate::ui::print_detail(&paths.pixi_home.display().to_string())?;
    let pixi = prepare_pixi(paths, core_root)?;
    transaction.record(Action::Created {
        path: pixi.path,
        identity: pixi.identity,
        recursive: true,
    });
    crate::ui::print_operation("Syncing packages")?;
    sync_pixi(paths)?;

    crate::ui::print_operation("Configuring shell and Git")?;
    crate::ui::print_detail(&home.join(".dotfiles").display().to_string())?;
    validate_private_source(core_root)?;
    prepare_backup_root(&paths.state_home)?;
    let dotfiles = home.join(".dotfiles");
    if let Some(moved) = backup_home_path(home, &paths.state_home, &dotfiles)? {
        record_backup(transaction, moved)?;
    }
    let dotfiles = install_dotfiles(core_root, home)?;
    crate::ui::print_detail("Installing dotfiles.")?;
    record_created(transaction, dotfiles.clone(), true)?;
    create_git_config(&dotfiles)?;
    let packages = stow_packages(paths, &dotfiles)?;
    for conflict in stow_conflicts(&dotfiles, home, &packages)? {
        if let Some(moved) = backup_home_path(home, &paths.state_home, &conflict)? {
            record_backup(transaction, moved)?;
        }
    }
    let stow = paths.pixi_home.join("bin/stow");
    crate::ui::print_detail("Linking configuration files.")?;
    for package in &packages {
        run_stow(paths, home, package)?;
        transaction.record(Action::Stowed {
            package: package.to_string_lossy().into_owned(),
            executable: stow.clone(),
            dir: dotfiles.clone(),
            target: home.to_path_buf(),
        });
    }
    validate_stowed_cshrc(&dotfiles, home)?;
    crate::ui::print_operation("Installing show-cad-status")?;
    crate::ui::print_detail(&release.tag_name)?;
    crate::ui::print_detail(&private_asset_url(release.show_cad_status_asset_id))?;
    crate::ui::print_detail(
        &home
            .join(".local/bin/show-cad-status")
            .display()
            .to_string(),
    )?;
    let status = install_status(
        paths,
        token,
        release.show_cad_status_asset_id,
        release.show_cad_status_checksum_asset_id,
    )?;
    crate::ui::print_operation("Verifying show-cad-status download")?;
    crate::ui::print_detail("SHA-256: verified")?;
    record_created(transaction, status, false)?;
    let status_entry = create_show_cad_status_entry(paths, home)?;
    record_created(transaction, status_entry, false)?;
    let config_parent = paths
        .eiyah_config
        .parent()
        .context("config path has no parent")?;
    for created in create_install_directories(config_parent)? {
        record_created(transaction, created, false)?;
    }
    crate::ui::print_operation("Creating Eiyah config")?;
    crate::ui::print_detail(&paths.eiyah_config.display().to_string())?;
    let config_publish = create_initial_config(paths)?;
    record_created(transaction, paths.eiyah_config.clone(), false)?;
    if let Some(error) = config_publish.cleanup_error {
        crate::ui::print_warning(&format!("failed to remove temporary files: {error}"));
    }
    crate::ui::print_operation("Verifying installation")?;
    validate_installation(paths, home)?;
    crate::ui::print_operation("Eiyah installation complete")?;
    Ok(())
}

// path identityを取得してCreated Actionを即時記録する
fn record_created(transaction: &mut Transaction, path: PathBuf, recursive: bool) -> Result<()> {
    let identity = PathIdentity::from_path(&path)?;
    transaction.record(Action::Created {
        path,
        identity,
        recursive,
    });
    Ok(())
}

// commit前にinitial install artifactのfilesystem形状と内容を検証する
fn validate_installation(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    validate_expected_executable(&paths.eiyah_prefix.join("bin/eiyah"), "Eiyah")?;
    validate_absolute_entry(
        &home.join(".local/bin/eiyah"),
        &paths.eiyah_prefix.join("bin/eiyah"),
    )?;
    let metadata_path = paths.eiyah_prefix.join("install.toml");
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
    if ResolvedPaths::from_install_metadata(metadata).map_err(|_| {
        anyhow::Error::new(crate::ui::UserFacingError::new(
            format!(
                "installation paths are invalid: {}",
                metadata_path.display()
            ),
            Vec::new(),
            Vec::new(),
        ))
    })? != *paths
    {
        return Err(crate::ui::UserFacingError::new(
            format!(
                "installation paths do not match installed files: {}",
                metadata_path.display()
            ),
            Vec::new(),
            Vec::new(),
        )
        .into());
    }
    validate_expected_executable(&paths.pixi_home.join("bin/pixi"), "Pixi")?;
    validate_regular_non_symlink(
        &paths.pixi_home.join("manifests/pixi-global.toml"),
        "Pixi manifest",
    )?;
    validate_non_symlink_directory(&home.join(".dotfiles"), "dotfiles")?;
    validate_stowed_cshrc(&home.join(".dotfiles"), home)?;
    validate_expected_executable(
        &paths.eiyah_prefix.join("bin/show-cad-status"),
        "show-cad-status",
    )?;
    validate_absolute_entry(
        &home.join(".local/bin/show-cad-status"),
        &paths.eiyah_prefix.join("bin/show-cad-status"),
    )?;
    if !load_config(&paths.eiyah_config)?.show_cad_status {
        bail!("initial config must enable show-cad-status");
    }
    Ok(())
}

// expected absolute symlink targetを検証する
pub(super) fn validate_absolute_entry(entry: &Path, expected: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(entry)?;
    if !metadata.file_type().is_symlink()
        || fs::read_link(entry)? != expected
        || !expected.is_absolute()
    {
        bail!("invalid Eiyah command link: {}", entry.display());
    }
    Ok(())
}

// non-symlink regular fileを検証する
pub(super) fn validate_regular_non_symlink(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    Ok(())
}

/// expected pathとpublic entryからinstall状態を判定する
fn detect_install_state(paths: &ResolvedPaths, public_entry: &Path) -> Result<InstallState> {
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
fn authorize_private_repository() -> Result<String> {
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
    write_device_instructions(
        &mut io::stdout().lock(),
        &device,
        crate::ui::stdout_style_enabled(),
    )?;
    let token = poll_device_token(&agent, &device, issued_at)?;
    crate::ui::print_detail("Authorization complete.")?;
    Ok(token)
}

/// authenticated GitHub APIからlatest stable Private Release情報を取得する
fn fetch_private_release(access_token: &str) -> Result<PrivateReleaseInfo> {
    let url = format!("https://api.github.com/repos/{PRIVATE_REPOSITORY}/releases/latest");
    let agent = http_agent();
    let mut response = private_request(&agent, &url, access_token)
        .call()
        .context("failed to fetch latest configuration release")?;
    let release = parse_private_release_response(response.body_mut())?;
    private_release_info(release)
}

// Private Release responseから取得に必要なfieldだけをdecodeする
fn parse_private_release_response(body: &mut ureq::Body) -> Result<PrivateReleaseResponse> {
    body.read_json()
        .context("failed to parse latest configuration release response")
}

// Device Flowのuser向けinstructionだけをstdoutへ出力する
fn write_device_instructions(
    output: &mut impl Write,
    device: &DeviceCodeResponse,
    styled: bool,
) -> Result<()> {
    write!(output, "First copy your one-time code: ")?;
    crate::ui::write_bold(output, &device.user_code, styled)?;
    writeln!(output)?;
    writeln!(
        output,
        "Then open {} in your browser.",
        device.verification_uri
    )?;
    writeln!(output, "Waiting for authorization...")?;
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
        bail!("latest configuration release is a draft");
    }
    if release.prerelease {
        bail!("latest configuration release is a prerelease");
    }
    if release.tag_name.is_empty() {
        bail!("latest configuration release tag is empty");
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
        anyhow::anyhow!("required configuration file is missing: {expected_name}")
    })?;
    if matches.next().is_some() {
        bail!("required configuration file is duplicated: {expected_name}");
    }
    Ok(id)
}

/// `$HOME`配下のed25519 key pairと`authorized_keys`を準備する
fn bootstrap_ssh(home: &Path) -> Result<SshSetupResult> {
    let user = env::var_os("USER").filter(|value| !value.is_empty());
    bootstrap_ssh_with(home, user.as_deref(), |command| command.output())
}

// ssh-keygen実行を差し替え可能にしてSSH bootstrapを行う
fn bootstrap_ssh_with(
    home: &Path,
    user: Option<&OsStr>,
    mut execute: impl FnMut(&mut Command) -> io::Result<Output>,
) -> Result<SshSetupResult> {
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
    let initial_state = (private_exists, public_exists);
    let key = match initial_state {
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

    let authorized_added = update_authorized_keys(&authorized_keys, &key)?;
    Ok(match (initial_state, authorized_added) {
        ((true, true), false) => SshSetupResult::ExistingAuthorized,
        ((true, false), authorization_added) => SshSetupResult::CreatedPublic {
            authorization_added,
        },
        ((false, false), authorization_added) => SshSetupResult::Generated {
            authorization_added,
        },
        ((true, true), true) => SshSetupResult::ExistingAuthorizedAdded,
        ((false, true), _) => unreachable!(),
    })
}

// SSH setup結果を利用者向けdetailへ変換する
fn print_ssh_setup(home: &Path, result: SshSetupResult) -> Result<()> {
    write_ssh_setup_with_residual_warning(&mut io::stdout().lock(), home, result)
}

// SSH変更後の表示failureへ残存Warningを付加する
fn write_ssh_setup_with_residual_warning(
    output: &mut impl Write,
    home: &Path,
    result: SshSetupResult,
) -> Result<()> {
    add_ssh_residual_warning(write_ssh_setup(output, home, result), result)
}

// SSH setup detailを指定outputへ書き出す
fn write_ssh_setup(output: &mut impl Write, home: &Path, result: SshSetupResult) -> Result<()> {
    let private_key = home.join(".ssh/id_ed25519");
    let public_key = home.join(".ssh/id_ed25519.pub");
    let authorized_keys = home.join(".ssh/authorized_keys");
    let existing = |output: &mut dyn Write| -> io::Result<()> {
        writeln!(output, "Using existing SSH key: {}", private_key.display())
    };
    match result {
        SshSetupResult::ExistingAuthorized => {
            existing(output)?;
            writeln!(output, "SSH key is already authorized.")?;
        }
        SshSetupResult::CreatedPublic {
            authorization_added,
        } => {
            existing(output)?;
            writeln!(output, "Created: {}", public_key.display())?;
            write_ssh_authorization(output, &authorized_keys, authorization_added)?;
        }
        SshSetupResult::Generated {
            authorization_added,
        } => {
            writeln!(
                output,
                "Generated ED25519 SSH key: {}",
                private_key.display()
            )?;
            write_ssh_authorization(output, &authorized_keys, authorization_added)?;
        }
        SshSetupResult::ExistingAuthorizedAdded => {
            existing(output)?;
            writeln!(output, "Added SSH key to: {}", authorized_keys.display())?;
        }
    }
    Ok(())
}

// authorizationを実際に追加したかに応じてSSH detailを表示する
fn write_ssh_authorization(
    output: &mut impl Write,
    authorized_keys: &Path,
    authorization_added: bool,
) -> Result<()> {
    if authorization_added {
        writeln!(output, "Added SSH key to: {}", authorized_keys.display())?;
    } else {
        writeln!(output, "SSH key is already authorized.")?;
    }
    Ok(())
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
fn update_authorized_keys(path: &Path, key: &SshPublicKey) -> Result<bool> {
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
        return Ok(false);
    }

    let mode = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.permissions().mode() & 0o7777,
        Err(error) if error.kind() == io::ErrorKind::NotFound => AUTHORIZED_KEYS_MODE,
        Err(error) => return Err(error.into()),
    };
    let temporary = authorized_keys_temporary_path(path);
    replace_authorized_keys(path, &temporary, &existing, key, mode)?;
    Ok(true)
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

// symlinkを追跡せずpath entryの存在を確認する
pub(super) fn path_exists(path: &Path) -> Result<bool> {
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

// Eiyahが作成したSSH fileへ規定permissionを適用する
fn set_file_mode(path: &Path, mode: u32) -> Result<()> {
    let mut permissions = fs::symlink_metadata(path)?.permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

// GitHubのlatest endpointからPublic Release responseを取得する

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use sha2::{Digest, Sha256};

    use crate::config::{InstallMetadata, save_install_metadata};
    use crate::lifecycle::test_support::*;
    use crate::transaction::read_backup_index;

    use super::*;

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
    // Device code responseとuser向け表示がsecretを含まないことを検証する
    fn parses_and_displays_device_code_response() -> Result<()> {
        let mut body = ureq::Body::builder().data(
            r#"{"device_code":"secret-device","user_code":"ABCD-1234","verification_uri":"https://github.com/login/device","expires_in":900,"interval":5}"#,
        );
        let device: DeviceCodeResponse = body.read_json()?;
        let mut output = Vec::new();
        write_device_instructions(&mut output, &device, false)?;
        let output = String::from_utf8(output)?;

        assert_eq!(device.expires_in, 900);
        assert_eq!(device.interval, 5);
        assert_eq!(
            output,
            "First copy your one-time code: ABCD-1234\n\
             Then open https://github.com/login/device in your browser.\n\
             Waiting for authorization...\n"
        );
        assert!(!output.contains(&device.device_code));

        let mut styled = Vec::new();
        write_device_instructions(&mut styled, &device, true)?;
        assert_eq!(
            String::from_utf8(styled)?,
            "First copy your one-time code: \x1b[1mABCD-1234\x1b[0m\n\
             Then open https://github.com/login/device in your browser.\n\
             Waiting for authorization...\n"
        );
        Ok(())
    }

    #[test]
    // SSH setupの各状態を利用者向けdetailへ変換する
    fn displays_ssh_setup_variants() -> Result<()> {
        let home = Path::new("/home/tester");
        let cases = [
            (
                SshSetupResult::ExistingAuthorized,
                "Using existing SSH key: /home/tester/.ssh/id_ed25519\nSSH key is already authorized.\n",
            ),
            (
                SshSetupResult::CreatedPublic {
                    authorization_added: true,
                },
                "Using existing SSH key: /home/tester/.ssh/id_ed25519\nCreated: /home/tester/.ssh/id_ed25519.pub\nAdded SSH key to: /home/tester/.ssh/authorized_keys\n",
            ),
            (
                SshSetupResult::CreatedPublic {
                    authorization_added: false,
                },
                "Using existing SSH key: /home/tester/.ssh/id_ed25519\nCreated: /home/tester/.ssh/id_ed25519.pub\nSSH key is already authorized.\n",
            ),
            (
                SshSetupResult::Generated {
                    authorization_added: true,
                },
                "Generated ED25519 SSH key: /home/tester/.ssh/id_ed25519\nAdded SSH key to: /home/tester/.ssh/authorized_keys\n",
            ),
            (
                SshSetupResult::Generated {
                    authorization_added: false,
                },
                "Generated ED25519 SSH key: /home/tester/.ssh/id_ed25519\nSSH key is already authorized.\n",
            ),
            (
                SshSetupResult::ExistingAuthorizedAdded,
                "Using existing SSH key: /home/tester/.ssh/id_ed25519\nAdded SSH key to: /home/tester/.ssh/authorized_keys\n",
            ),
        ];
        for (result, expected) in cases {
            let mut output = Vec::new();
            write_ssh_setup(&mut output, home, result)?;
            assert_eq!(String::from_utf8(output)?, expected);
        }
        Ok(())
    }

    #[test]
    // SSH変更後のdetail出力failureへ残存Warningを付加する
    fn reports_ssh_residual_state_after_output_failure() -> Result<()> {
        struct FailingWriter;

        impl Write for FailingWriter {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "output failed"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = write_ssh_setup_with_residual_warning(
            &mut FailingWriter,
            Path::new("/home/tester"),
            SshSetupResult::Generated {
                authorization_added: true,
            },
        )
        .unwrap_err();
        let mut output = Vec::new();
        crate::ui::write_error_report(&mut output, &error, false)?;
        assert_eq!(
            String::from_utf8(output)?,
            "Error: output failed\nWarning: SSH changes made during setup were not reverted.\n"
        );
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

        let result = bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| {
            Ok(ssh_keygen_output(
                0,
                b" ssh-ed25519 AAAA derived-comment \n",
                b"",
            ))
        })?;

        assert_eq!(
            result,
            SshSetupResult::CreatedPublic {
                authorization_added: true
            }
        );
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
    // public keyのみ作成し既存authorizationを重複追加しない
    fn derives_public_key_without_duplicate_authorization() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = ssh_home(&directory)?;
        let ssh = home.join(".ssh");
        fs::create_dir(&ssh)?;
        fs::write(ssh.join("id_ed25519"), b"private")?;
        fs::write(
            ssh.join("authorized_keys"),
            b"ssh-ed25519 AAAA existing-comment\n",
        )?;

        let result = bootstrap_ssh_with(&home, Some(OsStr::new("user")), |_| {
            Ok(ssh_keygen_output(
                0,
                b"ssh-ed25519 AAAA derived-comment\n",
                b"",
            ))
        })?;

        assert_eq!(
            result,
            SshSetupResult::CreatedPublic {
                authorization_added: false
            }
        );
        assert_eq!(
            fs::read(ssh.join("authorized_keys"))?,
            b"ssh-ed25519 AAAA existing-comment\n"
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

    #[test]
    // identity確定失敗をprepare boundary内でowned PIXI_HOME cleanupへ含める
    fn cleans_prepared_pixi_home_when_identity_capture_fails() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        create_core_manifest(&core_root, b"version = 1\n")?;

        let result = prepare_pixi_with(
            &paths,
            &core_root,
            |_| Ok(b"installer".to_vec()),
            |_| Ok(()),
            |_, _| {
                create_test_pixi_binary(&paths)?;
                Ok(ExitStatus::from_raw(0))
            },
            |_| Ok(ssh_keygen_output(0, b"pixi 0.50.0\n", b"")),
            |_| Err(anyhow::anyhow!("injected identity failure")),
        );

        assert!(result.is_err());
        assert!(!paths.pixi_home.exists());
        Ok(())
    }

    #[test]
    // failed Stow package cleanup errorよりoriginal Stow failureを先頭に保持する
    fn keeps_original_stow_failure_primary_when_cleanup_fails() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let dotfiles = home.join(".dotfiles");
        fs::create_dir_all(&dotfiles)?;
        let stow = paths.pixi_home.join("bin/stow");
        fs::create_dir_all(stow.parent().unwrap())?;
        fs::write(&stow, b"stow")?;
        fs::set_permissions(&stow, fs::Permissions::from_mode(0o755))?;

        let error = run_stow_package_with(
            &paths,
            &home,
            OsStr::new("git"),
            |_| Ok(ExitStatus::from_raw(1 << 8)),
            |_| Ok(ExitStatus::from_raw(2 << 8)),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.starts_with("Stow failed:"));
        assert!(message.contains("failed Stow cleanup:"));
        Ok(())
    }

    #[test]
    // lock後state再確認をNotInstalled・Installed・Partialの各分岐で行う
    fn routes_install_state_inside_existing_lock() -> Result<()> {
        use std::cell::RefCell;

        let events = RefCell::new(Vec::new());
        route_install_state(
            InstallState::Installed,
            || {
                events.borrow_mut().push("lock");
                Ok(())
            },
            || unreachable!(),
            || {
                events.borrow_mut().push("update");
                Ok(())
            },
            || unreachable!(),
        )?;
        assert_eq!(*events.borrow(), ["lock", "update"]);

        for (locked_state, expected) in [
            (InstallState::NotInstalled, "install"),
            (InstallState::Installed, "update"),
        ] {
            let events = RefCell::new(Vec::new());
            route_install_state(
                InstallState::NotInstalled,
                || {
                    events.borrow_mut().push("lock");
                    Ok(())
                },
                || {
                    events.borrow_mut().push("detect");
                    Ok(locked_state)
                },
                || {
                    events.borrow_mut().push("update");
                    Ok(())
                },
                || {
                    events.borrow_mut().push("install");
                    Ok(())
                },
            )?;
            assert_eq!(*events.borrow(), ["lock", "detect", expected]);
        }

        let events = RefCell::new(Vec::new());
        assert!(
            route_install_state(
                InstallState::NotInstalled,
                || {
                    events.borrow_mut().push("lock");
                    Ok(())
                },
                || {
                    events.borrow_mut().push("detect");
                    Ok(InstallState::Partial)
                },
                || unreachable!(),
                || unreachable!(),
            )
            .is_err()
        );
        assert_eq!(*events.borrow(), ["lock", "detect"]);

        assert!(
            route_install_state(
                InstallState::Partial,
                || -> Result<()> { unreachable!() },
                || unreachable!(),
                || unreachable!(),
                || unreachable!(),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    // existing installのoperationをupdate前に設計どおり表示する
    fn displays_existing_install_before_update() -> Result<()> {
        let (result, output) = crate::ui::capture_stdout(|| {
            route_install_state(
                InstallState::Installed,
                || Ok(()),
                || unreachable!(),
                || Ok(()),
                || unreachable!(),
            )
        });

        result?;
        assert_eq!(output, "\n==> Eiyah is already installed\n");
        Ok(())
    }

    #[test]
    // initial installのproduction出力経路を完走してexact transcriptを検証する
    fn displays_initial_install_transcript() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let core_root = directory.path.join("core");
        fs::create_dir(&home)?;
        for root in [
            &paths.config_home,
            &paths.data_home,
            &paths.state_home,
            &paths.cache_home,
        ] {
            fs::create_dir(root)?;
        }
        fs::create_dir_all(core_root.join("dotfiles/git/.config/git"))?;
        fs::create_dir_all(core_root.join("dotfiles/tcsh"))?;
        fs::write(core_root.join("dotfiles/tcsh/.cshrc"), b"cshrc")?;
        let release = PrivateReleaseInfo {
            tag_name: "v1.2.3".to_owned(),
            archive_url: "https://example.com/configuration.tar.gz".to_owned(),
            show_cad_status_asset_id: 101,
            show_cad_status_checksum_asset_id: 102,
        };
        let mut transaction = Transaction::new();

        let (result, output) = crate::ui::capture_stdout(|| {
            install_not_installed_with(
                &paths,
                &home,
                &core_root,
                "token",
                &release,
                &mut transaction,
                |paths, _| {
                    fs::create_dir_all(paths.pixi_home.join("bin"))?;
                    fs::create_dir_all(paths.pixi_home.join("manifests"))?;
                    for binary in ["pixi", "stow"] {
                        let path = paths.pixi_home.join("bin").join(binary);
                        fs::write(&path, b"binary")?;
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                    }
                    fs::write(
                        paths.pixi_home.join("manifests/pixi-global.toml"),
                        b"version = 1\n",
                    )?;
                    Ok(CreatedManagedRoot {
                        path: paths.pixi_home.clone(),
                        identity: PathIdentity::from_path(&paths.pixi_home)?,
                    })
                },
                |_| Ok(()),
                |dotfiles| {
                    let target = dotfiles.join("git/.config/git/config.local");
                    fs::write(&target, b"[user]\n")?;
                    Ok(target)
                },
                |_, home, package| {
                    if package == OsStr::new("tcsh") {
                        symlink(".dotfiles/tcsh/.cshrc", home.join(".cshrc"))?;
                    }
                    Ok(())
                },
                |paths, _, _, _| {
                    let target = paths.eiyah_prefix.join("bin/show-cad-status");
                    fs::write(&target, b"binary")?;
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o755))?;
                    Ok(target)
                },
            )
        });

        result?;
        let expected = format!(
            "\n==> Installing Eiyah\n{}\n\n==> Installing Pixi\n{}\n\n==> Syncing packages\n\n==> Configuring shell and Git\n{}\nInstalling dotfiles.\nLinking configuration files.\n\n==> Installing show-cad-status\nv1.2.3\n{}\n{}\n\n==> Verifying show-cad-status download\nSHA-256: verified\n\n==> Creating Eiyah config\n{}\n\n==> Verifying installation\n\n==> Eiyah installation complete\n",
            home.join(".local/bin/eiyah").display(),
            paths.pixi_home.display(),
            home.join(".dotfiles").display(),
            private_asset_url(release.show_cad_status_asset_id),
            home.join(".local/bin/show-cad-status").display(),
            paths.eiyah_config.display(),
        );
        assert_eq!(output, expected);
        Ok(())
    }

    #[test]
    // XDG相当のinitial pathが変わってもinstalled metadataのpathとlockを使用する
    fn routes_installed_operation_with_metadata_paths_after_xdg_change() -> Result<()> {
        let directory = TestDirectory::new()?;
        let installed_paths = fixture_paths(&directory.path.join("installed"))?;
        let changed_xdg_paths = fixture_paths(&directory.path.join("changed-xdg"))?;
        let home = directory.path.join("home");
        let public_entry = home.join(".local/bin/eiyah");
        create_installed_fixture(&installed_paths, &public_entry)?;
        let initial_resolver_called = std::cell::Cell::new(false);

        let selected_paths = resolve_install_paths_with(&home, || {
            initial_resolver_called.set(true);
            Ok(changed_xdg_paths.clone())
        })?;
        let state = detect_install_state(&selected_paths, &public_entry)?;

        assert!(!initial_resolver_called.get());
        assert_eq!(selected_paths, installed_paths);
        assert_eq!(state, InstallState::Installed);
        route_install_state(
            state,
            || LockGuard::acquire(&selected_paths.state_home),
            || unreachable!(),
            || {
                assert_eq!(selected_paths, installed_paths);
                Ok(())
            },
            || unreachable!(),
        )?;
        assert!(installed_paths.state_home.join("eiyah/lock").is_file());
        assert!(!changed_xdg_paths.state_home.join("eiyah/lock").exists());
        Ok(())
    }

    #[test]
    // public entryがない未install時だけenvironment相当のinitial pathを使用する
    fn resolves_environment_paths_only_for_initial_install() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let environment_paths = fixture_paths(&directory.path.join("environment"))?;
        let resolver_called = std::cell::Cell::new(false);

        let selected_paths = resolve_install_paths_with(&home, || {
            resolver_called.set(true);
            Ok(environment_paths.clone())
        })?;
        let state = detect_install_state(&selected_paths, &home.join(".local/bin/eiyah"))?;

        assert!(resolver_called.get());
        assert_eq!(selected_paths, environment_paths);
        assert_eq!(state, InstallState::NotInstalled);
        Ok(())
    }

    #[test]
    // existing public entryのmetadata discovery failureをinitial installへfallbackしない
    fn rejects_invalid_existing_public_entry_as_partial_install() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let public_entry = home.join(".local/bin/eiyah");
        fs::create_dir_all(public_entry.parent().unwrap())?;
        fs::write(&public_entry, b"not a symlink")?;
        let resolver_called = std::cell::Cell::new(false);

        let error = resolve_install_paths_with(&home, || {
            resolver_called.set(true);
            fixture_paths(&directory.path.join("environment"))
        })
        .unwrap_err();

        assert!(!resolver_called.get());
        assert!(format!("{error:#}").starts_with("existing Eiyah installation is incomplete:"));
        Ok(())
    }

    #[test]
    // operation失敗時は即時記録済みActionを逆順rollback後にarchive cleanupする
    fn rolls_back_actions_before_archive_cleanup_on_install_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let managed = directory.path.join("managed");
        let child = managed.join("artifact");
        let cleanup_observed = std::cell::Cell::new(false);
        let mut transaction = Transaction::new();

        let error = complete_install_transaction(
            &mut transaction,
            |transaction| {
                fs::create_dir(&managed)?;
                record_created(transaction, managed.clone(), false)?;
                fs::write(&child, b"artifact")?;
                record_created(transaction, child.clone(), false)?;
                bail!("install validation failed")
            },
            || {
                assert!(!child.exists());
                assert!(!managed.exists());
                cleanup_observed.set(true);
                Ok(())
            },
            |_| unreachable!(),
        )
        .unwrap_err();

        assert!(format!("{error:#}").starts_with("install validation failed"));
        assert!(cleanup_observed.get());
        Ok(())
    }

    #[test]
    // Pixi prepare成功直後のActionをsync failure時にtransaction rollbackする
    fn rolls_back_prepared_pixi_after_sync_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let core_root = directory.path.join("core");
        create_core_manifest(&core_root, b"version = 1\n")?;
        let mut transaction = Transaction::new();

        let error = complete_install_transaction(
            &mut transaction,
            |transaction| {
                let pixi = prepare_pixi_with(
                    &paths,
                    &core_root,
                    |_| Ok(b"installer".to_vec()),
                    |_| Ok(()),
                    |_, _| {
                        create_test_pixi_binary(&paths)?;
                        Ok(ExitStatus::from_raw(0))
                    },
                    |_| Ok(ssh_keygen_output(0, b"pixi 0.50.0\n", b"")),
                    PathIdentity::from_path,
                )?;
                transaction.record(Action::Created {
                    path: pixi.path,
                    identity: pixi.identity,
                    recursive: true,
                });
                bail!("pixi global sync failed")
            },
            || {
                assert!(!paths.pixi_home.exists());
                Ok(())
            },
            |_| unreachable!(),
        )
        .unwrap_err();

        assert!(format!("{error:#}").starts_with("pixi global sync failed"));
        Ok(())
    }

    #[test]
    // Stow途中失敗では先行packageだけをTransaction rollbackへ委ねる
    fn rolls_back_prior_stow_package_after_later_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let executable = directory.path.join("stow");
        let log = directory.path.join("unstow.log");
        fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' \"$6\" >> {}\n", log.display()),
        )?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
        let mut transaction = Transaction::new();

        assert!(
            complete_install_transaction(
                &mut transaction,
                |transaction| {
                    transaction.record(Action::Stowed {
                        package: "git".to_owned(),
                        executable: executable.clone(),
                        dir: directory.path.clone(),
                        target: directory.path.clone(),
                    });
                    bail!("later Stow package failed")
                },
                || Ok(()),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(log)?, "git\n");
        Ok(())
    }

    #[test]
    // validation failureをrollbackしcommit後cleanup failureだけWarningへ変換する
    fn applies_validation_commit_and_cleanup_boundaries() -> Result<()> {
        let directory = TestDirectory::new()?;
        let invalid = directory.path.join("invalid");
        let mut transaction = Transaction::new();
        assert!(
            complete_install_transaction(
                &mut transaction,
                |transaction| {
                    fs::write(&invalid, b"invalid")?;
                    record_created(transaction, invalid.clone(), false)?;
                    bail!("install validation failed")
                },
                || Ok(()),
                |_| unreachable!(),
            )
            .is_err()
        );
        assert!(!invalid.exists());

        let committed = directory.path.join("committed");
        let warning = std::cell::RefCell::new(String::new());
        complete_install_transaction(
            &mut transaction,
            |transaction| {
                fs::write(&committed, b"committed")?;
                record_created(transaction, committed.clone(), false)
            },
            || bail!("archive cleanup failed"),
            |message| *warning.borrow_mut() = message.to_owned(),
        )?;
        assert!(committed.exists());
        assert_eq!(
            *warning.borrow(),
            "failed to remove temporary files: archive cleanup failed"
        );
        Ok(())
    }

    #[test]
    // SSH残存変更とrollback failureをprimary Error後のWarningとして表示する
    fn reports_install_residual_state() -> Result<()> {
        let directory = TestDirectory::new()?;
        let replaced = directory.path.join("replaced");
        fs::write(&replaced, b"owned")?;
        let identity = PathIdentity::from_path(&replaced)?;
        let mut transaction = Transaction::new();
        transaction.record(Action::Created {
            path: replaced.clone(),
            identity,
            recursive: false,
        });
        fs::rename(&replaced, directory.path.join("original"))?;
        fs::write(&replaced, b"replacement")?;

        let error = complete_install_transaction(
            &mut transaction,
            |_| bail!("installation failed"),
            || Ok(()),
            |_| unreachable!(),
        )
        .unwrap_err();
        let error = add_ssh_residual_warning(
            Err(error),
            SshSetupResult::Generated {
                authorization_added: true,
            },
        )
        .unwrap_err();
        let mut output = Vec::new();
        crate::ui::write_error_report(&mut output, &error, false)?;
        assert_eq!(
            String::from_utf8(output)?,
            "Error: installation failed\n\
             Warning: Eiyah could not fully restore the previous system state.\n\
             Warning: SSH changes made during setup were not reverted.\n"
        );
        Ok(())
    }

    #[test]
    // dotfiles有無に依存せず固定backup rootを先に作成・検証する
    fn prepares_private_environment_backup_root_without_dotfiles() -> Result<()> {
        let directory = TestDirectory::new()?;
        let state_home = directory.path.join("state");
        fs::create_dir(&state_home)?;
        prepare_backup_root(&state_home)?;
        let backup_root = state_home.join("eiyah/backup/home");
        assert!(backup_root.is_dir());
        assert_eq!(
            fs::metadata(backup_root)?.permissions().mode() & 0o777,
            BACKUP_DIRECTORY_MODE
        );
        Ok(())
    }

    #[test]
    // Private sourceを検証してdotfilesのmode・content・symlinkを保持してcopyする
    fn installs_private_dotfiles_tree() -> Result<()> {
        let directory = TestDirectory::new()?;
        let core = directory.path.join("core");
        let source = core.join("dotfiles/git");
        fs::create_dir_all(&source)?;
        let source_file = source.join("config");
        fs::write(&source_file, b"git config")?;
        fs::set_permissions(&source_file, fs::Permissions::from_mode(0o640))?;
        symlink("config", source.join("config-link"))?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;

        let target = install_dotfiles(&core, &home)?;

        assert_eq!(target, home.join(".dotfiles"));
        assert_eq!(fs::read(target.join("git/config"))?, b"git config");
        assert_eq!(
            fs::metadata(target.join("git/config"))?
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::read_link(target.join("git/config-link"))?,
            Path::new("config")
        );
        assert_eq!(fs::read(&source_file)?, b"git config");
        Ok(())
    }

    #[test]
    // copy中に他者所有entryが出現してもそのentryをcleanupしない
    fn preserves_dotfiles_entry_created_during_copy_race() -> Result<()> {
        let directory = TestDirectory::new()?;
        let core = directory.path.join("core");
        fs::create_dir_all(core.join("dotfiles/git"))?;
        fs::write(core.join("dotfiles/git/config"), b"source")?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let raced_target = home.join(".dotfiles/git/config");

        assert!(
            install_dotfiles_with(&core, &home, |target| {
                if target == raced_target {
                    fs::write(target, b"concurrent")?;
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&raced_target)?, b"concurrent");
        Ok(())
    }

    #[test]
    // HOME pathをrelative layoutのbackupへ移動しcollision時はsourceを維持する
    fn backs_up_home_paths_without_replacement() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let state = directory.path.join("state");
        fs::create_dir(&home)?;
        fs::create_dir(&state)?;
        let source = home.join(".cshrc");
        fs::write(&source, b"original")?;

        let moved = backup_home_path(&home, &state, &source)?.unwrap();
        assert_eq!(moved.from, source);
        assert_eq!(moved.to, state.join("eiyah/backup/home/.cshrc"));
        assert_eq!(moved.index, state.join("eiyah/backup/index"));
        assert_eq!(moved.entry, encode_backup_index_entry(Path::new(".cshrc"))?);
        assert_eq!(fs::read(&moved.to)?, b"original");
        assert_eq!(
            read_backup_index(&moved.index)?,
            Some(vec![moved.entry.clone()])
        );

        fs::write(&source, b"second")?;
        assert!(backup_home_path(&home, &state, &source).is_err());
        assert_eq!(fs::read(&source)?, b"second");
        assert_eq!(fs::read(&moved.to)?, b"original");
        assert!(backup_home_path(&home, &state, &directory.path.join("outside")).is_err());
        Ok(())
    }

    #[test]
    // backup後のdetail出力failureでも記録済みActionがHOMEとindexをrollbackする
    fn rolls_back_backup_after_detail_output_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let state = directory.path.join("state");
        fs::create_dir(&home)?;
        fs::create_dir(&state)?;
        let source = home.join(".cshrc");
        let target = state.join("eiyah/backup/home/.cshrc");
        let index = state.join("eiyah/backup/index");
        fs::write(&source, b"original")?;
        let mut transaction = Transaction::new();

        let error = complete_install_transaction(
            &mut transaction,
            |transaction| {
                let moved = backup_home_path(&home, &state, &source)?.unwrap();
                record_backup_with(transaction, moved, |_| {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "output failed"))
                })
            },
            || Ok(()),
            |_| unreachable!(),
        )
        .unwrap_err();

        assert!(format!("{error:#}").starts_with("output failed"));
        assert_eq!(fs::read(&source)?, b"original");
        assert!(!target.exists());
        assert_eq!(
            crate::transaction::read_backup_index(&index)?,
            Some(Vec::new())
        );
        Ok(())
    }

    #[test]
    // index更新failure時はbackupをHOMEへ戻しrestore failureをcontextへ残す
    fn restores_unindexed_backup_after_index_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let state = directory.path.join("state");
        fs::create_dir(&home)?;
        fs::create_dir(&state)?;
        let source = home.join(".cshrc");
        let target = state.join("eiyah/backup/home/.cshrc");
        fs::write(&source, b"original")?;

        assert!(
            backup_home_path_with(&home, &state, &source, |_, _| {
                Err(anyhow::anyhow!("injected index failure"))
            })
            .is_err()
        );
        assert_eq!(fs::read(&source)?, b"original");
        assert!(!target.exists());

        let error = backup_home_path_with(&home, &state, &source, |_, _| {
            fs::write(&source, b"collision")?;
            Err(anyhow::anyhow!("injected index failure"))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("failed to restore unindexed backup"));
        assert_eq!(fs::read(&source)?, b"collision");
        assert_eq!(fs::read(&target)?, b"original");
        Ok(())
    }

    #[test]
    // existing backup ancestorがsymlinkの場合はbackup前に拒否する
    fn rejects_symlink_backup_ancestor() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let state = directory.path.join("state");
        let redirected = directory.path.join("redirected");
        fs::create_dir(&home)?;
        fs::create_dir(&state)?;
        fs::create_dir_all(redirected.join("backup/home"))?;
        symlink(&redirected, state.join("eiyah"))?;
        let source = home.join(".cshrc");
        fs::write(&source, b"original")?;

        assert!(backup_home_path(&home, &state, &source).is_err());
        assert_eq!(fs::read(&source)?, b"original");
        assert!(!redirected.join("backup/home/.cshrc").exists());
        Ok(())
    }

    #[test]
    // Private archive pathのsingle top-levelとtraversal contractを検証する
    fn validates_private_archive_entry_paths() -> Result<()> {
        let mut top = None;
        validate_archive_entry(Path::new("root/dotfiles/config"), &mut top)?;
        validate_archive_entry(Path::new("root/pixi/manifest"), &mut top)?;
        assert_eq!(top, Some(OsString::from("root")));
        assert!(validate_archive_entry(Path::new("other/file"), &mut top).is_err());

        let mut top = None;
        assert!(validate_archive_entry(Path::new("/absolute"), &mut top).is_err());
        assert!(validate_archive_entry(Path::new("root/../outside"), &mut top).is_err());
        assert!(validate_archive_entry(Path::new(""), &mut top).is_err());
        Ok(())
    }

    #[test]
    // local tar fixtureでinspection・strip extraction・unsupported symlink拒否を検証する
    fn inspects_and_extracts_private_archive() -> Result<()> {
        let directory = TestDirectory::new()?;
        let staging = directory.path.join("staging");
        let release = staging.join("release-root");
        fs::create_dir_all(release.join("dotfiles"))?;
        fs::write(release.join("dotfiles/config"), b"content")?;
        let archive = directory.path.join("valid.tar.gz");
        let status = Command::new(TAR_PATH)
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .arg("release-root")
            .status()?;
        assert!(status.success());
        let core = directory.path.join("core");
        extract_private_archive(&archive, &core)?;
        assert_eq!(fs::read(core.join("dotfiles/config"))?, b"content");

        symlink("config", release.join("dotfiles/link"))?;
        let invalid = directory.path.join("invalid.tar.gz");
        let status = Command::new(TAR_PATH)
            .arg("-czf")
            .arg(&invalid)
            .arg("-C")
            .arg(&staging)
            .arg("release-root")
            .status()?;
        assert!(status.success());
        assert!(inspect_archive(&invalid).is_err());
        Ok(())
    }

    #[test]
    // commit前validationが全managed artifactとinitial configを確認する
    fn validates_completed_installation() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir_all(paths.eiyah_prefix.join("bin"))?;
        fs::create_dir_all(paths.pixi_home.join("bin"))?;
        fs::create_dir_all(paths.pixi_home.join("manifests"))?;
        fs::create_dir_all(home.join(".local/bin"))?;
        fs::create_dir_all(home.join(".dotfiles/tcsh"))?;
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;
        for binary in [
            paths.eiyah_prefix.join("bin/eiyah"),
            paths.eiyah_prefix.join("bin/show-cad-status"),
            paths.pixi_home.join("bin/pixi"),
        ] {
            fs::write(&binary, b"binary")?;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        }
        fs::write(
            paths.pixi_home.join("manifests/pixi-global.toml"),
            b"version = 1\n",
        )?;
        fs::write(home.join(".dotfiles/tcsh/.cshrc"), b"cshrc")?;
        symlink(
            &paths.eiyah_prefix.join("bin/eiyah"),
            home.join(".local/bin/eiyah"),
        )?;
        symlink(
            &paths.eiyah_prefix.join("bin/show-cad-status"),
            home.join(".local/bin/show-cad-status"),
        )?;
        symlink(".dotfiles/tcsh/.cshrc", home.join(".cshrc"))?;
        save_install_metadata(&paths)?;
        fs::write(&paths.eiyah_config, b"show-cad-status = true\n")?;

        validate_installation(&paths, &home)?;
        fs::write(&paths.eiyah_config, b"show-cad-status = false\n")?;
        assert!(validate_installation(&paths, &home).is_err());
        Ok(())
    }

    #[test]
    // Git identityからmode 0600のconfig.localを生成しmissing identityを拒否する
    fn creates_generated_git_config_local() -> Result<()> {
        let directory = TestDirectory::new()?;
        let dotfiles = directory.path.join("dotfiles");
        fs::create_dir_all(dotfiles.join("git/.config/git"))?;
        let target = create_git_config_local_with(&dotfiles, |key| {
            let value = match key {
                "user.name" => b"Example User\n".as_slice(),
                "user.email" => b"user@example.com\n".as_slice(),
                _ => unreachable!(),
            };
            Ok(ssh_keygen_output(0, value, b""))
        })?;
        assert_eq!(
            fs::read_to_string(&target)?,
            "[user]\n    name = Example User\n    email = user@example.com\n"
        );
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o600);

        fs::remove_file(&target)?;
        assert!(
            create_git_config_local_with(&dotfiles, |_| Ok(ssh_keygen_output(0, b"\n", b"")))
                .is_err()
        );
        assert!(!target.exists());
        Ok(())
    }

    #[test]
    // Stow packageをbyte順に列挙しexpected executable以外を許可しない
    fn enumerates_stow_packages_in_stable_order() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let stow = paths.pixi_home.join("bin/stow");
        fs::create_dir_all(stow.parent().unwrap())?;
        fs::write(&stow, b"stow")?;
        fs::set_permissions(&stow, fs::Permissions::from_mode(0o755))?;
        let dotfiles = directory.path.join("dotfiles");
        fs::create_dir(&dotfiles)?;
        fs::create_dir(dotfiles.join("zsh"))?;
        fs::create_dir(dotfiles.join("git"))?;
        fs::write(dotfiles.join("README"), b"ignored")?;
        symlink("git", dotfiles.join("linked-package"))?;

        assert_eq!(
            stow_packages(&paths, &dotfiles)?,
            vec![OsString::from("git"), OsString::from("zsh")]
        );
        Ok(())
    }

    #[test]
    // correct symlinkを維持し通常targetと.cshrc conflictをgenericに収集する
    fn detects_stow_conflicts_and_correct_links() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let dotfiles = home.join(".dotfiles");
        fs::create_dir_all(dotfiles.join("git/.config/git"))?;
        fs::create_dir_all(dotfiles.join("tcsh"))?;
        fs::write(dotfiles.join("git/.config/git/config"), b"source")?;
        fs::write(dotfiles.join("tcsh/.cshrc"), b"source")?;
        fs::create_dir_all(home.join(".config/git"))?;
        symlink(
            "../../.dotfiles/git/.config/git/config",
            home.join(".config/git/config"),
        )?;
        fs::write(home.join(".cshrc"), b"existing")?;

        let conflicts = stow_conflicts(
            &dotfiles,
            &home,
            &[OsString::from("git"), OsString::from("tcsh")],
        )?;
        assert_eq!(conflicts, vec![home.join(".cshrc")]);
        let state = directory.path.join("state");
        fs::create_dir(&state)?;
        let moved = backup_home_path(&home, &state, &conflicts[0])?.unwrap();
        assert_eq!(moved.to, state.join("eiyah/backup/home/.cshrc"));
        symlink(".dotfiles/tcsh/.cshrc", home.join(".cshrc"))?;
        validate_stowed_cshrc(&dotfiles, &home)?;
        assert!(is_correct_stow_symlink(
            &home.join(".config/git/config"),
            &dotfiles.join("git/.config/git/config")
        )?);
        Ok(())
    }

    #[test]
    // Stowをcanonical argv・cwdで実行しnon-zeroをerrorにする
    fn runs_stow_with_canonical_command() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let stow = paths.pixi_home.join("bin/stow");
        fs::create_dir_all(stow.parent().unwrap())?;
        fs::write(&stow, b"stow")?;
        fs::set_permissions(&stow, fs::Permissions::from_mode(0o755))?;
        let home = directory.path.join("home");
        fs::create_dir_all(home.join(".dotfiles"))?;
        let packages = vec![OsString::from("git"), OsString::from("tcsh")];

        run_stow_with(&paths, &home, &packages, |command| {
            assert_eq!(command.get_program(), stow);
            assert_eq!(
                command.get_current_dir(),
                Some(home.join(".dotfiles").as_path())
            );
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [
                    OsStr::new("--target"),
                    home.as_os_str(),
                    OsStr::new("--dir"),
                    home.join(".dotfiles").as_os_str(),
                    OsStr::new("git"),
                    OsStr::new("tcsh")
                ]
            );
            Ok(std::process::ExitStatus::from_raw(0))
        })?;
        assert!(
            run_stow_with(&paths, &home, &packages, |_| {
                Ok(std::process::ExitStatus::from_raw(1 << 8))
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    // show-cad-status asset ID・checksum・mode・public symlink contractを検証する
    fn installs_show_cad_status_and_public_entry() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(paths.eiyah_prefix.join("bin"))?;
        let binary = b"show-cad-status binary";
        let digest = Sha256::digest(binary);
        let checksum = format!("{digest:x}  {SHOW_CAD_STATUS_ASSET_NAME}\n").into_bytes();

        let target = install_show_cad_status_with(
            &paths,
            101,
            102,
            |id, file| {
                assert_eq!(id, 101);
                file.write_all(binary)?;
                Ok(())
            },
            |id| {
                assert_eq!(id, 102);
                Ok(checksum.clone())
            },
        )?;
        assert_eq!(fs::read(&target)?, binary);
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o755);
        assert_eq!(
            private_asset_url(101),
            "https://api.github.com/repos/su-ito-lab/eiyah-core/releases/assets/101"
        );

        let home = directory.path.join("home");
        fs::create_dir_all(home.join(".local/bin"))?;
        let entry = create_show_cad_status_entry(&paths, &home)?;
        assert_eq!(fs::read_link(&entry)?, target);
        assert!(create_show_cad_status_entry(&paths, &home).is_err());
        Ok(())
    }

    #[test]
    // show-cad-status checksum mismatchとdownload failureでpartial targetをcleanupする
    fn cleans_failed_show_cad_status_installation() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        fs::create_dir_all(paths.eiyah_prefix.join("bin"))?;
        let digest = Sha256::digest(b"different");
        let checksum = format!("{digest:x}  {SHOW_CAD_STATUS_ASSET_NAME}\n").into_bytes();

        assert!(
            install_show_cad_status_with(
                &paths,
                1,
                2,
                |_, file| {
                    file.write_all(b"binary")?;
                    Ok(())
                },
                |_| Ok(checksum.clone()),
            )
            .is_err()
        );
        assert!(!paths.eiyah_prefix.join("bin/show-cad-status").exists());

        assert!(
            install_show_cad_status_with(
                &paths,
                1,
                2,
                |_, file| {
                    file.write_all(b"partial")?;
                    Err(anyhow::anyhow!("injected download failure"))
                },
                |_| Ok(checksum.clone()),
            )
            .is_err()
        );
        assert!(!paths.eiyah_prefix.join("bin/show-cad-status").exists());
        assert!(parse_show_cad_status_checksum(b"invalid\n").is_err());
        Ok(())
    }
}
