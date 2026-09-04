// ==================================================
// @file src/lifecycle/uninstall.rs
// @brief Managed environment removal and backup restoration
// ==================================================

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use anyhow::{Context, Result, bail};

use crate::config::{
    ResolvedPaths, discover_install_metadata, load_install_metadata, runtime_home,
};
use crate::transaction::{LockGuard, PathIdentity, decode_backup_index_entry, read_backup_index};

use super::install::{
    INSTALL_DIRECTORY_MODE, path_exists, rename_without_replace, stow_packages,
    validate_absolute_entry, validate_expected_executable, validate_non_symlink_directory,
    validate_regular_non_symlink,
};

// --------------------------------------------------
// Managed Environment Removal
// --------------------------------------------------

// managed pathに要求するfilesystem形状
enum ManagedRemovalKind<'a> {
    // expected absolute targetを持つsymlink
    ExactSymlink(&'a Path),
    // symlinkではないregular file
    RegularFile,
    // symlinkではないdirectory tree
    Directory,
}

/// managed Stow packageを逆順にすべてunstowする
fn unstow_managed_packages(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    unstow_managed_packages_with(paths, home, |command| command.status().map_err(Into::into))
}

// Stow実行を差し替え可能にして全packageのfailureを集約する
fn unstow_managed_packages_with(
    paths: &ResolvedPaths,
    home: &Path,
    mut execute: impl FnMut(&mut Command) -> Result<ExitStatus>,
) -> Result<()> {
    let executable = paths.pixi_home.join("bin/stow");
    let dotfiles = home.join(".dotfiles");
    validate_non_symlink_directory(&dotfiles, "dotfiles")?;
    let mut packages = stow_packages(paths, &dotfiles)?;
    packages.reverse();

    let mut failures = Vec::new();
    for package in packages {
        let mut command = Command::new(&executable);
        command
            .arg("--delete")
            .arg("--target")
            .arg(home)
            .arg("--dir")
            .arg(&dotfiles)
            .arg(&package)
            .current_dir(&dotfiles)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        match execute(&mut command) {
            Ok(status) if status.success() => {}
            Ok(status) => failures.push(format!("{}: {status}", package.to_string_lossy())),
            Err(error) => failures.push(format!("{}: {error:#}", package.to_string_lossy())),
        }
    }
    if !failures.is_empty() {
        bail!("failed to unstow managed packages: {}", failures.join("; "));
    }
    Ok(())
}

/// expected show-cad-status public entryだけを削除する
fn remove_show_cad_status_entry(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    let entry = home.join(".local/bin/show-cad-status");
    let target = paths.eiyah_prefix.join("bin/show-cad-status");
    remove_managed_path(&entry, ManagedRemovalKind::ExactSymlink(&target))
}

/// persistent managed contentを仕様順に削除する
fn remove_managed_content(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    let targets = [
        (
            paths.eiyah_prefix.join("bin/show-cad-status"),
            ManagedRemovalKind::RegularFile,
        ),
        (paths.eiyah_config.clone(), ManagedRemovalKind::RegularFile),
        (home.join(".dotfiles"), ManagedRemovalKind::Directory),
        (paths.pixi_home.clone(), ManagedRemovalKind::Directory),
        (
            paths.eiyah_prefix.join("install.toml"),
            ManagedRemovalKind::RegularFile,
        ),
    ];
    for (index, (path, kind)) in targets.into_iter().enumerate() {
        match index {
            1 => {
                crate::ui::print_operation("Removing Eiyah configuration")?;
                crate::ui::print_detail(&paths.eiyah_config.display().to_string())?;
                crate::ui::print_detail(&home.join(".dotfiles").display().to_string())?;
            }
            3 => {
                crate::ui::print_operation("Removing Pixi environment")?;
                crate::ui::print_detail(&paths.pixi_home.display().to_string())?;
            }
            _ => {}
        }
        remove_managed_path(&path, kind)?;
    }
    Ok(())
}

/// Eiyah cache namespaceだけを削除する
fn cleanup_uninstall_cache(paths: &ResolvedPaths) -> Result<()> {
    remove_managed_path(
        &paths.cache_home.join("eiyah"),
        ManagedRemovalKind::Directory,
    )
}

// managed pathを削除直前にidentity再確認して他者所有pathを保護する
fn remove_managed_path(path: &Path, kind: ManagedRemovalKind<'_>) -> Result<()> {
    remove_managed_path_with(path, kind, |_| Ok(()))
}

// identity確認間のraceをtest可能にしてmanaged pathを削除する
fn remove_managed_path_with(
    path: &Path,
    kind: ManagedRemovalKind<'_>,
    before_recheck: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    match kind {
        ManagedRemovalKind::ExactSymlink(expected) => {
            if !metadata.file_type().is_symlink()
                || !expected.is_absolute()
                || fs::read_link(path)? != expected
            {
                bail!("invalid managed symlink: {}", path.display());
            }
        }
        ManagedRemovalKind::RegularFile => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                bail!("managed path must be a regular file: {}", path.display());
            }
        }
        ManagedRemovalKind::Directory => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!("managed path must be a directory: {}", path.display());
            }
        }
    }

    let identity = PathIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    before_recheck(path)?;
    if PathIdentity::from_path(path)? != identity {
        bail!("managed path identity changed: {}", path.display());
    }
    match kind {
        ManagedRemovalKind::Directory => fs::remove_dir_all(path)?,
        ManagedRemovalKind::ExactSymlink(_) | ManagedRemovalKind::RegularFile => {
            fs::remove_file(path)?
        }
    }
    Ok(())
}

// --------------------------------------------------
// Backup Restoration
// --------------------------------------------------

// indexから生成した1件のrestore plan
#[derive(Clone, Debug, Eq, PartialEq)]
struct BackupRestoreEntry {
    // HOMEからのrelative path
    relative: PathBuf,
}

/// backup indexを正本としてHOME contentを復元する
fn restore_home_backups(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    restore_home_backups_with(paths, home, |_, _| Ok(()), |_| Ok(()))
}

// restore直前とindex削除直前のraceをtest可能にしてbackupを復元する
fn restore_home_backups_with(
    paths: &ResolvedPaths,
    home: &Path,
    mut before_restore: impl FnMut(&Path, &Path) -> Result<()>,
    before_index_remove: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let backup_directory = paths.state_home.join("eiyah/backup");
    let backup_root = backup_directory.join("home");
    let index = backup_directory.join("index");
    let Some(entries) = read_backup_index(&index)? else {
        return Ok(());
    };
    let plan = backup_restore_plan(&entries)?;
    if !plan.is_empty() {
        validate_non_symlink_directory(&backup_root, "backup root")?;
        crate::ui::print_operation("Restoring previous configuration")?;
    }

    for restore in &plan {
        let source = backup_root.join(&restore.relative);
        let destination = home.join(&restore.relative);
        if !destination.starts_with(home) {
            bail!("backup restore destination escapes HOME");
        }
        let result = (|| -> Result<()> {
            validate_backup_source_parents(
                &backup_root,
                source
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("backup source has no parent"))?,
            )?;
            restore_backup_entry(&source, &destination, home, |source, destination| {
                before_restore(source, destination)
            })
        })();
        if let Err(error) = result {
            bail!(
                "failed to restore {}: {error:#}",
                restore.relative.as_os_str().to_string_lossy()
            );
        }
        crate::ui::print_detail(&destination.display().to_string())?;
    }

    remove_backup_index_with(&index, before_index_remove)?;
    cleanup_backup_scaffold(&backup_root)?;
    Ok(())
}

// validated index entryからdepth・byte順のrestore planを構成する
fn backup_restore_plan(entries: &[Vec<u8>]) -> Result<Vec<BackupRestoreEntry>> {
    let mut plan = Vec::with_capacity(entries.len());
    for encoded in entries {
        let relative = normalize_relative_backup_path(&decode_backup_index_entry(encoded)?)?;
        if plan
            .iter()
            .any(|existing: &BackupRestoreEntry| existing.relative == relative)
        {
            bail!("backup index contains a duplicate path");
        }
        plan.push(BackupRestoreEntry { relative });
    }
    for (index, entry) in plan.iter().enumerate() {
        if plan.iter().enumerate().any(|(other_index, other)| {
            index != other_index
                && entry.relative != other.relative
                && entry.relative.starts_with(&other.relative)
        }) {
            bail!("backup index contains ancestor and descendant paths");
        }
    }
    plan.sort_by(|left, right| {
        left.relative
            .components()
            .count()
            .cmp(&right.relative.components().count())
            .then_with(|| {
                left.relative
                    .as_os_str()
                    .as_bytes()
                    .cmp(right.relative.as_os_str().as_bytes())
            })
    });
    Ok(plan)
}

// separator表現を統一してHOME外へ出ないrelative pathを返す
fn normalize_relative_backup_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            bail!("backup path must contain only relative normal components");
        };
        normalized.push(component);
    }
    if normalized.as_os_str().is_empty() {
        bail!("backup path must not be empty");
    }
    Ok(normalized)
}

// 1件のbackup sourceをidentity確認後にHOMEへatomic no-replaceで復元する
fn restore_backup_entry(
    source: &Path,
    destination: &Path,
    home: &Path,
    before_rename: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let file_type = metadata.file_type();
    if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
        bail!("backup source has unsupported type: {}", source.display());
    }
    match fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => bail!(
            "backup restore destination already exists: {}",
            destination.display()
        ),
        Err(error) => return Err(error.into()),
    }
    create_restore_directories(
        home,
        destination
            .parent()
            .ok_or_else(|| anyhow::anyhow!("backup restore destination has no parent"))?,
    )?;
    let identity = PathIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    before_rename(source, destination)?;
    if PathIdentity::from_path(source)? != identity {
        bail!("backup source identity changed: {}", source.display());
    }
    rename_without_replace(source, destination)?;
    Ok(())
}

// backup rootからsource parentまでsymlinkを含まないdirectory chainを検証する
fn validate_backup_source_parents(root: &Path, parent: &Path) -> Result<()> {
    let relative = parent
        .strip_prefix(root)
        .with_context(|| format!("backup source parent escapes root: {}", parent.display()))?;
    let mut directory = root.to_path_buf();
    validate_non_symlink_directory(&directory, "backup source parent")?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("backup source parent has an invalid component");
        };
        directory.push(component);
        validate_non_symlink_directory(&directory, "backup source parent")?;
    }
    Ok(())
}

// HOME配下のmissing restore parentをmode 0755で作成する
fn create_restore_directories(home: &Path, parent: &Path) -> Result<()> {
    validate_non_symlink_directory(home, "HOME")?;
    let relative = parent
        .strip_prefix(home)
        .with_context(|| format!("restore parent must be under HOME: {}", parent.display()))?;
    let mut directory = home.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("restore parent has an invalid component");
        };
        directory.push(component);
        let mut builder = DirBuilder::new();
        builder.mode(INSTALL_DIRECTORY_MODE);
        match builder.create(&directory) {
            Ok(()) => fs::set_permissions(
                &directory,
                fs::Permissions::from_mode(INSTALL_DIRECTORY_MODE),
            )?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_non_symlink_directory(&directory, "restore parent")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

// restore成功時だけidentity一致を確認してbackup indexを削除する
fn remove_backup_index_with(
    index: &Path,
    before_remove: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(index)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("backup index must be a regular file: {}", index.display());
    }
    let identity = PathIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    before_remove(index)?;
    if PathIdentity::from_path(index)? != identity {
        bail!("backup index identity changed: {}", index.display());
    }
    fs::remove_file(index)?;
    Ok(())
}

// backup root配下のempty scaffold directoryだけを深い順に削除する
fn cleanup_backup_scaffold(root: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("backup scaffold must be a directory: {}", root.display());
    }
    cleanup_backup_directory(root)?;
    Ok(())
}

// child directoryを先に処理してemptyになったscaffoldだけを削除する
fn cleanup_backup_directory(directory: &Path) -> Result<bool> {
    let mut empty = true;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("backup scaffold contains a symlink: {}", path.display());
        }
        if metadata.file_type().is_dir() {
            if !cleanup_backup_directory(&path)? {
                empty = false;
            }
        } else {
            empty = false;
        }
    }
    if empty {
        fs::remove_dir(directory)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// --------------------------------------------------
// Uninstall Orchestration
// --------------------------------------------------

/// install metadata由来pathだけを使用してmanaged environmentを削除する
pub(crate) fn run_uninstall(cleanup_plan: &Path) -> Result<()> {
    let home = runtime_home()?;
    run_uninstall_from_home(&home, cleanup_plan)
}

// HOMEを差し替え可能にしてmetadata復元からlock・uninstallまで実行する
fn run_uninstall_from_home(home: &Path, cleanup_plan: &Path) -> Result<()> {
    let paths = load_uninstall_paths(home)?;
    let _lock = LockGuard::acquire(&paths.state_home)?;
    uninstall_locked(&paths, home, cleanup_plan)
}

// public entryからinstalled metadataを読みuninstall対象pathを復元する
fn load_uninstall_paths(home: &Path) -> Result<ResolvedPaths> {
    let metadata_path = discover_install_metadata(&home.join(".local/bin/eiyah"))?;
    let metadata = load_install_metadata(&metadata_path)?;
    ResolvedPaths::from_install_metadata(metadata)
}

// lock取得後のuninstall処理を仕様順に実行する
fn uninstall_locked(paths: &ResolvedPaths, home: &Path, cleanup_plan: &Path) -> Result<()> {
    uninstall_preflight(paths, home)?;
    write_final_cleanup_plan(paths, home, cleanup_plan)?;
    crate::ui::print_operation("Unlinking configuration files")?;
    unstow_managed_packages(paths, home)?;
    crate::ui::print_operation("Removing show-cad-status")?;
    crate::ui::print_detail(
        &home
            .join(".local/bin/show-cad-status")
            .display()
            .to_string(),
    )?;
    remove_show_cad_status_entry(paths, home)?;
    remove_managed_content(paths, home)?;
    restore_home_backups(paths, home)?;
    validate_uninstallation(paths, home)?;
    cleanup_uninstall_cache(paths)
}

// installed metadata由来pathをshell向けfixed-order protocolへ記録する
fn write_final_cleanup_plan(paths: &ResolvedPaths, home: &Path, target: &Path) -> Result<()> {
    if !target.is_absolute() {
        bail!("final cleanup plan path must be absolute");
    }
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("final cleanup plan path has no parent"))?;
    validate_non_symlink_directory(parent, "final cleanup plan parent")?;
    match fs::symlink_metadata(target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => bail!("final cleanup plan already exists: {}", target.display()),
        Err(error) => return Err(error.into()),
    }

    let eiyah_binary = paths.eiyah_prefix.join("bin/eiyah");
    let eiyah_entry = home.join(".local/bin/eiyah");
    let state_root = paths.state_home.join("eiyah");
    let lock = state_root.join("lock");
    let content = format!(
        "eiyah-binary={}\neiyah-entry={}\nstate-root={}\nlock={}\n",
        encode_path(&eiyah_binary),
        encode_path(&eiyah_entry),
        encode_path(&state_root),
        encode_path(&lock),
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(target)
        .with_context(|| format!("failed to create final cleanup plan {}", target.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

// Unix path bytesをlowercase hexadecimalへencodeする
fn encode_path(path: &Path) -> String {
    let bytes = path.as_os_str().as_bytes();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

/// filesystemを変更せずuninstall開始条件を検証する
fn uninstall_preflight(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    if home.as_os_str().is_empty() || !home.is_absolute() {
        bail!("HOME must be an absolute non-empty path");
    }
    for (name, path) in [
        ("config home", &paths.config_home),
        ("data home", &paths.data_home),
        ("state home", &paths.state_home),
        ("cache home", &paths.cache_home),
        ("Eiyah prefix", &paths.eiyah_prefix),
        ("Eiyah config", &paths.eiyah_config),
        ("Pixi home", &paths.pixi_home),
    ] {
        if path.as_os_str().is_empty() || !path.is_absolute() {
            bail!(
                "{name} must be an absolute non-empty path: {}",
                path.display()
            );
        }
    }

    validate_non_symlink_directory(&home.join(".dotfiles"), "dotfiles")?;
    validate_non_symlink_directory(&paths.pixi_home, "Pixi home")?;
    validate_expected_executable(&paths.pixi_home.join("bin/stow"), "Stow")?;
    validate_optional_directory(&paths.state_home.join("eiyah/backup"), "backup directory")?;
    validate_optional_uninstall_file(&paths.state_home.join("eiyah/backup/index"), "backup index")
}

/// managed content削除とbackup復元の完了状態を変更せず検証する
fn validate_uninstallation(paths: &ResolvedPaths, home: &Path) -> Result<()> {
    for path in [
        home.join(".local/bin/show-cad-status"),
        paths.eiyah_prefix.join("bin/show-cad-status"),
        paths.eiyah_config.clone(),
        home.join(".dotfiles"),
        paths.pixi_home.clone(),
        paths.eiyah_prefix.join("install.toml"),
        paths.state_home.join("eiyah/backup/index"),
    ] {
        if path_exists(&path)? {
            bail!("uninstall target still exists: {}", path.display());
        }
    }

    validate_expected_executable(&paths.eiyah_prefix.join("bin/eiyah"), "Eiyah")?;
    validate_absolute_entry(
        &home.join(".local/bin/eiyah"),
        &paths.eiyah_prefix.join("bin/eiyah"),
    )?;
    validate_non_symlink_directory(&paths.state_home.join("eiyah"), "state root")?;
    validate_regular_non_symlink(&paths.state_home.join("eiyah/lock"), "operation lock")?;

    let backup_root = paths.state_home.join("eiyah/backup/home");
    match fs::symlink_metadata(&backup_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) => {
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                bail!(
                    "backup root must be a non-symlink directory: {}",
                    backup_root.display()
                );
            }
            if fs::read_dir(&backup_root)?.next().transpose()?.is_some() {
                bail!("backup root must be empty: {}", backup_root.display());
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

// optional pathがdirectory / non-symlinkであることを検証する
fn validate_optional_directory(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => bail!(
            "{label} must be a non-symlink directory: {}",
            path.display()
        ),
        Err(error) => Err(error.into()),
    }
}

// optional pathがregular / non-symlinkであることを検証する
fn validate_optional_uninstall_file(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(metadata) if metadata.file_type().is_file() && !metadata.file_type().is_symlink() => {
            Ok(())
        }
        Ok(_) => bail!("{label} must be a regular file: {}", path.display()),
        Err(error) => Err(error.into()),
    }
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::ffi::{CString, OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::os::unix::process::ExitStatusExt;

    use crate::lifecycle::test_support::*;
    use crate::transaction::{encode_backup_index_entry, write_backup_index};

    use super::*;

    #[test]
    // non-UTF-8を含むUnix path bytesをlowercase hexへencodeする
    fn encodes_cleanup_plan_path_bytes() {
        let path = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        assert_eq!(encode_path(&path), "2fff");
    }

    // fixtureのbackup indexとsource rootを作成する
    fn create_backup_fixture(
        paths: &ResolvedPaths,
        entries: &[&Path],
    ) -> Result<(PathBuf, PathBuf)> {
        let backup = paths.state_home.join("eiyah/backup");
        let root = backup.join("home");
        fs::create_dir_all(&root)?;
        let encoded = entries
            .iter()
            .map(|entry| encode_backup_index_entry(entry))
            .collect::<Result<Vec<_>>>()?;
        let index = backup.join("index");
        write_backup_index(&index, &encoded)?;
        Ok((root, index))
    }

    // Checkpoint Cのuninstall全体を実行できるmanaged fixtureを作成する
    fn create_uninstall_fixture(paths: &ResolvedPaths, home: &Path) -> Result<()> {
        let public_entry = home.join(".local/bin/eiyah");
        create_installed_fixture(paths, &public_entry)?;

        let status_binary = paths.eiyah_prefix.join("bin/show-cad-status");
        fs::write(&status_binary, b"status")?;
        fs::set_permissions(&status_binary, fs::Permissions::from_mode(0o755))?;
        symlink(&status_binary, home.join(".local/bin/show-cad-status"))?;
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;
        fs::write(&paths.eiyah_config, b"show-cad-status = true\n")?;
        fs::create_dir_all(home.join(".dotfiles/tcsh"))?;
        let stow = paths.pixi_home.join("bin/stow");
        fs::create_dir_all(stow.parent().unwrap())?;
        fs::write(&stow, b"#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&stow, fs::Permissions::from_mode(0o755))?;

        let relative = Path::new(".cshrc");
        let (backup_root, _) = create_backup_fixture(paths, &[relative])?;
        fs::write(backup_root.join(relative), b"restored")?;
        fs::create_dir_all(paths.cache_home.join("eiyah/downloads"))?;
        fs::write(paths.cache_home.join("eiyah/downloads/archive"), b"cache")?;
        fs::create_dir_all(paths.state_home.join("eiyah"))?;
        fs::write(paths.state_home.join("eiyah/lock"), b"")?;
        Ok(())
    }

    #[test]
    // managed packageを逆byte順・canonical argvで全件unstowしfailureを集約する
    fn unstows_all_managed_packages_in_reverse_order() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let stow = paths.pixi_home.join("bin/stow");
        fs::create_dir_all(stow.parent().unwrap())?;
        fs::write(&stow, b"stow")?;
        fs::set_permissions(&stow, fs::Permissions::from_mode(0o755))?;
        let home = directory.path.join("home");
        let dotfiles = home.join(".dotfiles");
        fs::create_dir_all(dotfiles.join("git"))?;
        fs::create_dir(dotfiles.join("tcsh"))?;
        fs::create_dir(dotfiles.join("zsh"))?;
        let mut visited = Vec::new();

        let error = unstow_managed_packages_with(&paths, &home, |command| {
            assert_eq!(command.get_program(), stow);
            assert_eq!(command.get_current_dir(), Some(dotfiles.as_path()));
            let arguments = command
                .get_args()
                .map(OsStr::to_os_string)
                .collect::<Vec<_>>();
            assert_eq!(
                &arguments[..5],
                [
                    OsString::from("--delete"),
                    OsString::from("--target"),
                    home.as_os_str().to_os_string(),
                    OsString::from("--dir"),
                    dotfiles.as_os_str().to_os_string(),
                ]
            );
            let package = arguments[5].clone();
            visited.push(package.clone());
            if package == OsStr::new("zsh") {
                Ok(std::process::ExitStatus::from_raw(9))
            } else {
                Ok(std::process::ExitStatus::from_raw(1 << 8))
            }
        })
        .unwrap_err();
        assert_eq!(
            visited,
            [
                OsString::from("zsh"),
                OsString::from("tcsh"),
                OsString::from("git")
            ]
        );
        let message = error.to_string();
        assert!(message.contains("zsh"));
        assert!(message.contains("tcsh"));
        assert!(message.contains("git"));
        assert!(dotfiles.exists());

        fs::remove_file(&stow)?;
        assert!(unstow_managed_packages_with(&paths, &home, |_| unreachable!()).is_err());
        Ok(())
    }

    #[test]
    // exact show-cad-status entryだけを削除しwrong targetを維持する
    fn removes_only_expected_show_cad_status_entry() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let entry = home.join(".local/bin/show-cad-status");
        let target = paths.eiyah_prefix.join("bin/show-cad-status");
        fs::create_dir_all(entry.parent().unwrap())?;
        symlink(&target, &entry)?;
        remove_show_cad_status_entry(&paths, &home)?;
        assert!(fs::symlink_metadata(&entry).is_err());
        remove_show_cad_status_entry(&paths, &home)?;

        symlink(paths.eiyah_prefix.join("bin/other"), &entry)?;
        assert!(remove_show_cad_status_entry(&paths, &home).is_err());
        assert!(fs::symlink_metadata(&entry)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    // managed contentだけを削除してuninstall後も保持するpathを変更しない
    fn removes_managed_content_in_eiyah_namespaces() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let binary = paths.eiyah_prefix.join("bin/show-cad-status");
        let eiyah = paths.eiyah_prefix.join("bin/eiyah");
        let metadata = paths.eiyah_prefix.join("install.toml");
        fs::create_dir_all(binary.parent().unwrap())?;
        fs::write(&binary, b"status")?;
        fs::write(&eiyah, b"eiyah")?;
        fs::write(&metadata, b"metadata")?;
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;
        fs::write(&paths.eiyah_config, b"config")?;
        fs::create_dir_all(home.join(".dotfiles/git"))?;
        fs::create_dir_all(paths.pixi_home.join("bin"))?;
        fs::create_dir_all(paths.state_home.join("eiyah/backup"))?;
        fs::create_dir_all(home.join(".ssh"))?;

        remove_managed_content(&paths, &home)?;

        for path in [
            binary,
            paths.eiyah_config.clone(),
            home.join(".dotfiles"),
            paths.pixi_home.clone(),
            metadata,
        ] {
            assert!(fs::symlink_metadata(path).is_err());
        }
        assert!(eiyah.exists());
        assert!(paths.eiyah_prefix.exists());
        assert!(paths.state_home.join("eiyah/backup").exists());
        assert!(home.join(".ssh").exists());
        Ok(())
    }

    #[test]
    // identity raceで置換された他者所有fileを削除しない
    fn preserves_replaced_managed_path_during_removal() -> Result<()> {
        let directory = TestDirectory::new()?;
        let target = directory.path.join("managed");
        let original = directory.path.join("original");
        fs::write(&target, b"original")?;

        let error = remove_managed_path_with(&target, ManagedRemovalKind::RegularFile, |path| {
            fs::rename(path, &original)?;
            fs::write(path, b"other owner")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        assert_eq!(fs::read(&target)?, b"other owner");
        assert_eq!(fs::read(&original)?, b"original");
        Ok(())
    }

    #[test]
    // managed pathのwrong file typeを拒否してexisting pathを維持する
    fn rejects_wrong_managed_removal_types() -> Result<()> {
        let directory = TestDirectory::new()?;
        let file = directory.path.join("file");
        let tree = directory.path.join("tree");
        fs::write(&file, b"file")?;
        fs::create_dir(&tree)?;

        assert!(remove_managed_path(&file, ManagedRemovalKind::Directory).is_err());
        assert!(remove_managed_path(&tree, ManagedRemovalKind::RegularFile).is_err());
        assert!(file.is_file());
        assert!(tree.is_dir());
        Ok(())
    }

    #[test]
    // cache namespaceだけを再帰削除してcache parentを保持する
    fn cleans_only_eiyah_cache_namespace() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let cache = paths.cache_home.join("eiyah");
        fs::create_dir_all(cache.join("downloads"))?;
        fs::write(cache.join("downloads/archive"), b"archive")?;
        fs::create_dir_all(paths.cache_home.join("other"))?;

        cleanup_uninstall_cache(&paths)?;

        assert!(fs::symlink_metadata(&cache).is_err());
        assert!(paths.cache_home.exists());
        assert!(paths.cache_home.join("other").exists());
        cleanup_uninstall_cache(&paths)?;

        symlink(paths.cache_home.join("other"), &cache)?;
        assert!(cleanup_uninstall_cache(&paths).is_err());
        assert!(fs::symlink_metadata(&cache)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    // identity raceで置換された他者所有directory treeを削除しない
    fn preserves_replaced_managed_directory_during_removal() -> Result<()> {
        let directory = TestDirectory::new()?;
        let target = directory.path.join("managed");
        let original = directory.path.join("original");
        fs::create_dir(&target)?;
        fs::write(target.join("owned"), b"owned")?;

        let error = remove_managed_path_with(&target, ManagedRemovalKind::Directory, |path| {
            fs::rename(path, &original)?;
            fs::create_dir(path)?;
            fs::write(path.join("other"), b"other owner")?;
            Ok(())
        })
        .unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        assert_eq!(fs::read(target.join("other"))?, b"other owner");
        assert_eq!(fs::read(original.join("owned"))?, b"owned");
        Ok(())
    }

    #[test]
    // missing backup indexをbackup対象なしとして扱う
    fn accepts_missing_backup_index() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        restore_home_backups(&paths, &home)?;
        assert!(home.is_dir());
        Ok(())
    }

    #[test]
    // indexをdepth・byte順へ変換しduplicate ownershipを拒否する
    fn builds_validated_backup_restore_plan() -> Result<()> {
        let entries = ["z/file", "a", "b/file"]
            .map(|path| encode_backup_index_entry(Path::new(path)))
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        let plan = backup_restore_plan(&entries)?;
        assert_eq!(
            plan.iter().map(|entry| &entry.relative).collect::<Vec<_>>(),
            [Path::new("a"), Path::new("b/file"), Path::new("z/file")]
        );

        let duplicate = encode_backup_index_entry(Path::new("same"))?;
        assert!(backup_restore_plan(&[duplicate.clone(), duplicate]).is_err());
        let parent = encode_backup_index_entry(Path::new("parent"))?;
        let child = encode_backup_index_entry(Path::new("parent/child"))?;
        assert!(backup_restore_plan(&[parent, child]).is_err());
        Ok(())
    }

    #[test]
    // malformed indexを全件検証してrestore開始前に拒否する
    fn rejects_invalid_backup_indexes_before_restore() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        let backup = paths.state_home.join("eiyah/backup");
        let index = backup.join("index");
        fs::create_dir_all(backup.join("home/valid"))?;
        fs::create_dir(&home)?;

        for content in [
            b"zz\n".as_slice(),
            b"2f6162736f6c757465\n".as_slice(),
            b"2e2e2f657363617065\n".as_slice(),
            b"76616c6964\n76616c6964\n".as_slice(),
            b"76616c6964\n7a00\n".as_slice(),
        ] {
            fs::write(&index, content)?;
            assert!(restore_home_backups(&paths, &home).is_err());
            assert!(!home.join("valid").exists());
        }
        Ok(())
    }

    #[test]
    // regular・directory・symlinkをmodeと内容を維持してHOMEへ復元する
    fn restores_all_supported_backup_types_and_cleans_scaffold() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let entries = [
            Path::new(".config/tool/config"),
            Path::new("saved-directory"),
            Path::new("saved-link"),
        ];
        let (root, index) = create_backup_fixture(&paths, &entries)?;
        fs::create_dir_all(root.join(".config/tool"))?;
        fs::write(root.join(".config/tool/config"), b"configuration")?;
        fs::set_permissions(
            root.join(".config/tool/config"),
            fs::Permissions::from_mode(0o640),
        )?;
        fs::create_dir(root.join("saved-directory"))?;
        fs::write(root.join("saved-directory/content"), b"directory")?;
        symlink("target", root.join("saved-link"))?;

        restore_home_backups(&paths, &home)?;

        assert_eq!(
            fs::read(home.join(".config/tool/config"))?,
            b"configuration"
        );
        assert_eq!(
            fs::metadata(home.join(".config/tool/config"))?
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(
            fs::read(home.join("saved-directory/content"))?,
            b"directory"
        );
        assert_eq!(fs::read_link(home.join("saved-link"))?, Path::new("target"));
        assert!(!index.exists());
        assert!(!root.exists());
        assert!(paths.state_home.join("eiyah/backup").is_dir());
        assert!(paths.state_home.join("eiyah").is_dir());
        Ok(())
    }

    #[test]
    // missing・special sourceとexisting destinationを変更せず拒否する
    fn rejects_invalid_backup_restore_sources_and_destinations() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let missing = Path::new("missing");
        let (root, index) = create_backup_fixture(&paths, &[missing])?;
        assert!(restore_home_backups(&paths, &home).is_err());
        assert!(index.exists());

        fs::remove_file(&index)?;
        let socket = Path::new("socket");
        write_backup_index(&index, &[encode_backup_index_entry(socket)?])?;
        let socket_path = root.join(socket);
        let socket_path_c = CString::new(socket_path.as_os_str().as_bytes())?;
        // SAFETY: pathはNUL終端済みでcall完了まで有効
        if unsafe { libc::mkfifo(socket_path_c.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        assert!(restore_home_backups(&paths, &home).is_err());
        assert!(index.exists());

        fs::remove_file(root.join(socket))?;
        fs::remove_file(&index)?;
        let collision = Path::new("collision");
        write_backup_index(&index, &[encode_backup_index_entry(collision)?])?;
        fs::write(root.join(collision), b"backup")?;
        fs::write(home.join(collision), b"existing")?;
        assert!(restore_home_backups(&paths, &home).is_err());
        assert_eq!(fs::read(home.join(collision))?, b"existing");
        assert_eq!(fs::read(root.join(collision))?, b"backup");
        Ok(())
    }

    #[test]
    // symlink parentを拒否しmissing parentだけをmode 0755で作成する
    fn validates_backup_restore_destination_parents() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let relative = Path::new(".config/tool/config");
        let (root, index) = create_backup_fixture(&paths, &[relative])?;
        fs::create_dir_all(root.join(".config/tool"))?;
        fs::write(root.join(relative), b"backup")?;
        let redirect = directory.path.join("redirect");
        fs::create_dir(&redirect)?;
        symlink(&redirect, home.join(".config"))?;
        assert!(restore_home_backups(&paths, &home).is_err());
        assert!(!redirect.join("tool/config").exists());
        assert!(index.exists());

        fs::remove_file(home.join(".config"))?;
        restore_home_backups(&paths, &home)?;
        assert_eq!(fs::read(home.join(relative))?, b"backup");
        assert_eq!(
            fs::metadata(home.join(".config/tool"))?
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        Ok(())
    }

    #[test]
    // destination・source replacement raceで他者pathを変更しない
    fn preserves_racing_paths_during_backup_restore() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let relative = Path::new("racing");
        let (root, index) = create_backup_fixture(&paths, &[relative])?;
        let source = root.join(relative);
        let destination = home.join(relative);
        fs::write(&source, b"backup")?;

        assert!(
            restore_home_backups_with(
                &paths,
                &home,
                |_, destination| {
                    fs::write(destination, b"other owner")?;
                    Ok(())
                },
                |_| Ok(()),
            )
            .is_err()
        );
        assert_eq!(fs::read(&destination)?, b"other owner");
        assert_eq!(fs::read(&source)?, b"backup");
        assert!(index.exists());

        fs::remove_file(&destination)?;
        let original = root.join("original");
        assert!(
            restore_home_backups_with(
                &paths,
                &home,
                |source, _| {
                    fs::rename(source, &original)?;
                    fs::write(source, b"replacement")?;
                    Ok(())
                },
                |_| Ok(()),
            )
            .is_err()
        );
        assert_eq!(fs::read(&source)?, b"replacement");
        assert_eq!(fs::read(&original)?, b"backup");
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    // restore failureで処理を停止し既restore pathとindex・remaining backupを保持する
    fn preserves_partial_restore_state_after_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let entries = [Path::new("a"), Path::new("b"), Path::new("c")];
        let (root, index) = create_backup_fixture(&paths, &entries)?;
        for entry in entries {
            fs::write(root.join(entry), entry.as_os_str().as_bytes())?;
        }
        fs::write(home.join("b"), b"collision")?;

        let error = restore_home_backups(&paths, &home).unwrap_err();
        assert!(error.to_string().contains("b"));
        assert_eq!(fs::read(home.join("a"))?, b"a");
        assert!(!root.join("a").exists());
        assert_eq!(fs::read(home.join("b"))?, b"collision");
        assert_eq!(fs::read(root.join("b"))?, b"b");
        assert_eq!(fs::read(root.join("c"))?, b"c");
        assert!(index.exists());
        Ok(())
    }

    #[test]
    // non-empty backup directoryを保持しscaffold symlinkをerrorにする
    fn preserves_non_empty_backup_state_during_scaffold_cleanup() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let (root, index) = create_backup_fixture(&paths, &[])?;
        fs::write(root.join("unindexed"), b"preserve")?;
        restore_home_backups(&paths, &home)?;
        assert!(!index.exists());
        assert_eq!(fs::read(root.join("unindexed"))?, b"preserve");

        let index = paths.state_home.join("eiyah/backup/index");
        write_backup_index(&index, &[])?;
        symlink("unindexed", root.join("link"))?;
        assert!(restore_home_backups(&paths, &home).is_err());
        assert!(!index.exists());
        assert!(
            fs::symlink_metadata(root.join("link"))?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    // index replacement raceで他者所有indexを削除しない
    fn preserves_replaced_backup_index_after_restore() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir(&home)?;
        let relative = Path::new("restored");
        let (root, index) = create_backup_fixture(&paths, &[relative])?;
        fs::write(root.join(relative), b"backup")?;
        let original_index = directory.path.join("original-index");

        assert!(
            restore_home_backups_with(
                &paths,
                &home,
                |_, _| Ok(()),
                |path| {
                    fs::rename(path, &original_index)?;
                    fs::write(path, b"other owner")?;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(fs::read(home.join(relative))?, b"backup");
        assert_eq!(fs::read(&index)?, b"other owner");
        assert!(original_index.is_file());
        Ok(())
    }

    #[test]
    // public entryのmetadataを正本としてuninstall pathを復元する
    fn loads_uninstall_paths_from_installed_metadata() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path.join("installed"))?;
        let home = directory.path.join("home");
        create_installed_fixture(&paths, &home.join(".local/bin/eiyah"))?;

        assert_eq!(load_uninstall_paths(&home)?, paths);
        Ok(())
    }

    #[test]
    // uninstall preflightがrequired path形状だけを変更せず検証する
    fn validates_uninstall_preflight_without_modification() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        fs::create_dir_all(home.join(".dotfiles/tcsh"))?;
        let stow = paths.pixi_home.join("bin/stow");
        fs::create_dir_all(stow.parent().unwrap())?;
        fs::write(&stow, b"stow")?;
        fs::set_permissions(&stow, fs::Permissions::from_mode(0o755))?;

        uninstall_preflight(&paths, &home)?;
        assert!(home.join(".dotfiles").is_dir());
        assert!(paths.pixi_home.is_dir());

        let backup = paths.state_home.join("eiyah/backup");
        fs::create_dir_all(backup.parent().unwrap())?;
        symlink(directory.path.join("elsewhere"), &backup)?;
        assert!(uninstall_preflight(&paths, &home).is_err());
        assert!(fs::symlink_metadata(&backup)?.file_type().is_symlink());
        Ok(())
    }

    #[test]
    // Checkpoint A/B helperを順序通り接続してstate・lockを保持する
    fn uninstalls_managed_environment_and_preserves_final_cleanup_targets() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        create_uninstall_fixture(&paths, &home)?;

        let cleanup_plan = directory.path.join("uninstall-cleanup-plan");
        let (result, output) =
            crate::ui::capture_stdout(|| run_uninstall_from_home(&home, &cleanup_plan));
        result?;

        assert_eq!(
            output,
            format!(
                "\n==> Unlinking configuration files\n\n==> Removing show-cad-status\n{}\n\n==> Removing Eiyah configuration\n{}\n{}\n\n==> Removing Pixi environment\n{}\n\n==> Restoring previous configuration\n{}\n",
                home.join(".local/bin/show-cad-status").display(),
                paths.eiyah_config.display(),
                home.join(".dotfiles").display(),
                paths.pixi_home.display(),
                home.join(".cshrc").display(),
            )
        );

        assert_eq!(fs::read(home.join(".cshrc"))?, b"restored");
        assert!(paths.eiyah_prefix.join("bin/eiyah").is_file());
        assert!(home.join(".local/bin/eiyah").is_symlink());
        assert!(paths.state_home.join("eiyah").is_dir());
        assert!(paths.state_home.join("eiyah/lock").is_file());
        assert!(!paths.cache_home.join("eiyah").exists());
        let expected = format!(
            "eiyah-binary={}\neiyah-entry={}\nstate-root={}\nlock={}\n",
            encode_path(&paths.eiyah_prefix.join("bin/eiyah")),
            encode_path(&home.join(".local/bin/eiyah")),
            encode_path(&paths.state_home.join("eiyah")),
            encode_path(&paths.state_home.join("eiyah/lock")),
        );
        assert_eq!(fs::read_to_string(&cleanup_plan)?, expected);
        assert_eq!(
            fs::metadata(&cleanup_plan)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    }

    #[test]
    // validation failureではcache cleanupへ進まず最終cleanup対象を保持する
    fn preserves_cache_when_uninstall_validation_fails() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        create_uninstall_fixture(&paths, &home)?;
        fs::remove_file(paths.eiyah_prefix.join("bin/eiyah"))?;

        assert!(
            uninstall_locked(
                &paths,
                &home,
                &directory.path.join("uninstall-cleanup-plan"),
            )
            .is_err()
        );
        assert!(paths.cache_home.join("eiyah/downloads/archive").is_file());
        assert_eq!(fs::read(home.join(".cshrc"))?, b"restored");
        assert!(paths.state_home.join("eiyah/lock").is_file());
        Ok(())
    }

    #[test]
    // invalidまたはexisting plan targetではdestructive uninstallを開始しない
    fn rejects_invalid_cleanup_plan_before_uninstall() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = fixture_paths(&directory.path)?;
        let home = directory.path.join("home");
        create_uninstall_fixture(&paths, &home)?;
        let existing = directory.path.join("existing-plan");
        fs::write(&existing, b"other owner")?;

        assert!(uninstall_locked(&paths, &home, Path::new("relative-plan")).is_err());
        assert!(uninstall_locked(&paths, &home, &existing).is_err());
        assert_eq!(fs::read(&existing)?, b"other owner");
        assert!(home.join(".dotfiles").is_dir());
        assert!(paths.pixi_home.is_dir());
        Ok(())
    }
}
