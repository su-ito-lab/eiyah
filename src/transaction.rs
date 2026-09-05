// ==================================================
// @file src/transaction.rs
// @brief Transaction action tracking and rollback
// ==================================================

use std::ffi::{CString, OsString};
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow, bail};

// backup index temporary file名をprocess内で一意にする連番
static BACKUP_INDEX_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
// backup indexとtemporary fileのpermission
const BACKUP_INDEX_MODE: u32 = 0o600;

/// rollback対象として記録する成功済みoperation
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// transaction中に新規作成したpath
    Created {
        /// 作成したpath
        path: PathBuf,
        /// Action記録時点のidentity
        identity: PathIdentity,
        /// managed root全体をrollback対象にするか
        recursive: bool,
    },
    /// transaction中に移動したpathと移動先
    Moved {
        /// move前のpath
        from: PathBuf,
        /// move後のpath
        to: PathBuf,
    },
    /// HOME backupと対応index entry
    BackedUp {
        /// backup前のHOME path
        from: PathBuf,
        /// backup tree内のpath
        to: PathBuf,
        /// backup index path
        index: PathBuf,
        /// lowercase hexでencodeしたindex entry
        entry: Vec<u8>,
    },
    /// transaction中にStowしたpackage名
    Stowed {
        /// Stow済みpackage名
        package: String,
        /// 実行に使用したabsolute Stow path
        executable: PathBuf,
        /// Stow source root
        dir: PathBuf,
        /// Stow target root
        target: PathBuf,
    },
}

/// rollback対象pathの作成時filesystem identity
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PathIdentity {
    /// Unix device number
    pub device: u64,
    /// Unix inode number
    pub inode: u64,
}

/// initial regular fileのpublish結果とbest-effort cleanup結果
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct InitialPublish {
    /// publish後に残ったtemporary pathのcleanup error
    pub(crate) cleanup_error: Option<String>,
}

impl PathIdentity {
    /// symlinkをfollowせず現在のpath identityを取得する
    pub fn from_path(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    // 記録済みidentityと現在のmetadataを比較する
    fn matches(self, metadata: &fs::Metadata) -> bool {
        self.device == metadata.dev() && self.inode == metadata.ino()
    }
}

/// 成功済みoperationをrollbackまで逆順に保持するtransaction
#[derive(Debug, Default)]
pub struct Transaction {
    // rollback対象となる成功済みaction
    actions: Vec<Action>,
}

/// operation全体の重複実行を防ぐexclusive lock
#[derive(Debug)]
pub struct LockGuard {
    // lockの生存期間中にflockを保持するfile handle
    file: fs::File,
}

impl Transaction {
    /// rollback対象を持たないtransactionを開始する
    pub fn new() -> Self {
        Self::default()
    }

    /// 成功済みoperationだけをrollback対象へ追加する
    pub fn record(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// 成功済みtransactionのactionをrollback対象から外す
    pub fn commit(&mut self) {
        self.actions.clear();
    }

    /// 記録済みactionを逆順にundoし、全失敗をまとめて返す
    pub fn rollback(&mut self) -> Result<()> {
        self.rollback_with(unstow_package)
    }

    // Stow解除処理を差し替え可能にして全actionのundoを継続する
    fn rollback_with(
        &mut self,
        mut unstow: impl FnMut(&str, &Path, &Path, &Path) -> Result<()>,
    ) -> Result<()> {
        // 失敗後も残りのundoを継続するため、各errorを最後まで保持する
        let mut errors = Vec::new();

        for action in self.actions.drain(..).rev() {
            if let Err(error) = undo(action, &mut unstow) {
                errors.push(error.to_string());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("rollback failed: {}", errors.join("; ")))
        }
    }
}

impl LockGuard {
    /// XDG state配下のlock fileへexclusive non-blocking lockを取得する
    pub fn acquire(state_home: &Path) -> Result<Self> {
        if state_home.as_os_str().is_empty() {
            bail!("state home must not be empty");
        }
        if !state_home.is_absolute() {
            bail!(
                "state home must be an absolute path: {}",
                state_home.display()
            );
        }

        // install前でもlock可能にするためのEiyah専用state directory
        let lock_directory = state_home.join("eiyah");
        fs::create_dir_all(&lock_directory)?;
        // process間で同一operation lockを共有する固定path
        let lock_path = lock_directory.join("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(&lock_path)?;

        // SAFETY: file descriptorは有効なopen fileを指し、LockGuardが所有する
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            let code = error.raw_os_error();
            if code == Some(libc::EWOULDBLOCK) || code == Some(libc::EAGAIN) {
                bail!("another Eiyah operation is already running.");
            }
            return Err(error.into());
        }

        Ok(Self { file })
    }
}

impl Drop for LockGuard {
    // guard終了時に明示的にlockを解放する
    fn drop(&mut self) {
        // SAFETY: file descriptorはDrop完了までLockGuardが所有する
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

// Action variantごとのfilesystemまたはStow変更を取り消す
fn undo(
    action: Action,
    unstow: &mut impl FnMut(&str, &Path, &Path, &Path) -> Result<()>,
) -> Result<()> {
    match action {
        Action::Created {
            path,
            identity,
            recursive,
        } => remove_created_path(&path, identity, recursive),
        Action::Moved { from, to } => restore_moved_path(&from, &to),
        Action::BackedUp {
            from,
            to,
            index,
            entry,
        } => rollback_backup(&from, &to, &index, &entry),
        Action::Stowed {
            package,
            executable,
            dir,
            target,
        } => unstow(&package, &executable, &dir, &target),
    }
}

// HOME backupを復元して対応index entryだけを削除する
fn rollback_backup(from: &Path, to: &Path, index: &Path, entry: &[u8]) -> Result<()> {
    rollback_backup_with(
        from,
        to,
        index,
        entry,
        |_| Ok(()),
        |path, entries| write_backup_index(path, entries).map(|_| ()),
    )
}

// source raceとindex更新failureをtest可能にしてHOME backupをrollbackする
fn rollback_backup_with(
    from: &Path,
    to: &Path,
    index: &Path,
    entry: &[u8],
    before_rename: impl FnOnce(&Path) -> Result<()>,
    update_index: impl FnOnce(&Path, &[Vec<u8>]) -> Result<()>,
) -> Result<()> {
    let metadata = fs::symlink_metadata(to)?;
    let file_type = metadata.file_type();
    if !(file_type.is_file() || file_type.is_dir() || file_type.is_symlink()) {
        bail!("backup source has unsupported type: {}", to.display());
    }
    match fs::symlink_metadata(from) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => bail!(
            "backup restore destination already exists: {}",
            from.display()
        ),
        Err(error) => return Err(error.into()),
    }
    let mut entries =
        read_backup_index(index)?.ok_or_else(|| anyhow!("backup index is missing"))?;
    if entries
        .iter()
        .filter(|candidate| candidate.as_slice() == entry)
        .count()
        != 1
    {
        bail!("backup index must contain the recorded entry exactly once");
    }
    let identity = PathIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    before_rename(to)?;
    if PathIdentity::from_path(to)? != identity {
        bail!("backup source identity changed: {}", to.display());
    }
    move_without_replace(to, from)?;
    entries.retain(|candidate| candidate.as_slice() != entry);
    update_index(index, &entries)?;
    Ok(())
}

/// HOME-relative path bytesをbackup index entryへencodeする
pub(crate) fn encode_backup_index_entry(path: &Path) -> Result<Vec<u8>> {
    validate_relative_backup_path(path)?;
    let mut encoded = Vec::with_capacity(path.as_os_str().as_bytes().len() * 2);
    for byte in path.as_os_str().as_bytes() {
        encoded.extend_from_slice(format!("{byte:02x}").as_bytes());
    }
    Ok(encoded)
}

/// backup index entryを検証済みHOME-relative pathへdecodeする
pub(crate) fn decode_backup_index_entry(entry: &[u8]) -> Result<PathBuf> {
    if entry.is_empty() || entry.len() % 2 != 0 || !entry.iter().all(u8::is_ascii_hexdigit) {
        bail!("backup index entry must be non-empty lowercase hexadecimal");
    }
    if entry.iter().any(u8::is_ascii_uppercase) {
        bail!("backup index entry must use lowercase hexadecimal");
    }
    let mut decoded = Vec::with_capacity(entry.len() / 2);
    for pair in entry.chunks_exact(2) {
        let text = std::str::from_utf8(pair)?;
        decoded.push(u8::from_str_radix(text, 16)?);
    }
    let path = PathBuf::from(OsString::from_vec(decoded));
    validate_relative_backup_path(&path)?;
    Ok(path)
}

// HOME-relative backup pathがabsoluteまたはdot componentを含まないことを保証する
fn validate_relative_backup_path(path: &Path) -> Result<()> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() || path.is_absolute() {
        bail!("backup path must be a non-empty relative path");
    }
    if bytes.contains(&0) {
        bail!("backup path must not contain a NUL byte");
    }
    if bytes
        .split(|byte| *byte == b'/')
        .any(|component| component == b"." || component == b"..")
    {
        bail!("backup path must not contain dot components");
    }
    Ok(())
}

/// backup indexを検証しencoded entryを順序通り返す
pub(crate) fn read_backup_index(path: &Path) -> Result<Option<Vec<Vec<u8>>>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("backup index must be a regular file: {}", path.display());
    }
    let content = fs::read(path)?;
    if !content.is_empty() && !content.ends_with(b"\n") {
        bail!("backup index entry must end with LF");
    }
    let mut entries = Vec::new();
    if content.is_empty() {
        return Ok(Some(entries));
    }
    for entry in content[..content.len().saturating_sub(1)].split(|byte| *byte == b'\n') {
        decode_backup_index_entry(entry)?;
        if entries.iter().any(|existing: &Vec<u8>| existing == entry) {
            bail!("backup index contains a duplicate entry");
        }
        entries.push(entry.to_vec());
    }
    Ok(Some(entries))
}

/// backup indexへ重複しないentryをatomicに追加する
pub(crate) fn append_backup_index_entry(path: &Path, entry: &[u8]) -> Result<InitialPublish> {
    decode_backup_index_entry(entry)?;
    let mut entries = read_backup_index(path)?.unwrap_or_default();
    if entries.iter().any(|existing| existing == entry) {
        bail!("backup index already contains the entry");
    }
    entries.push(entry.to_vec());
    write_backup_index(path, &entries)
}

// backup index全体をsame-directory temporary fileからatomic置換する
pub(crate) fn write_backup_index(path: &Path, entries: &[Vec<u8>]) -> Result<InitialPublish> {
    write_backup_index_with(path, entries, |_| Ok(()))
}

// rename直前のtemporary replacement raceをtest可能にしてindexを保存する
fn write_backup_index_with(
    path: &Path,
    entries: &[Vec<u8>],
    before_rename: impl FnOnce(&Path) -> Result<()>,
) -> Result<InitialPublish> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("backup index has no parent"))?;
    let mut temporary_name = OsString::from(".index.tmp-");
    temporary_name.push(format!(
        "{}-{}",
        std::process::id(),
        BACKUP_INDEX_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = parent.join(temporary_name);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(BACKUP_INDEX_MODE)
        .open(&temporary)?;
    let created = file.metadata()?;
    let result = (|| -> Result<InitialPublish> {
        file.set_permissions(fs::Permissions::from_mode(BACKUP_INDEX_MODE))?;
        for entry in entries {
            decode_backup_index_entry(entry)?;
            file.write_all(entry)?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        before_rename(&temporary)?;
        let current = fs::symlink_metadata(&temporary)?;
        if current.dev() != created.dev() || current.ino() != created.ino() {
            bail!("backup index temporary file was replaced");
        }
        let published = match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                    bail!("backup index must be a regular file: {}", path.display());
                }
                fs::rename(&temporary, path)?;
                InitialPublish {
                    cleanup_error: None,
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => publish_initial_file(
                &temporary,
                path,
                PathIdentity {
                    device: created.dev(),
                    inode: created.ino(),
                },
            )?,
            Err(error) => return Err(error.into()),
        };
        Ok(published)
    })();
    if result.is_err() {
        if let Ok(current) = fs::symlink_metadata(&temporary)
            && current.dev() == created.dev()
            && current.ino() == created.ino()
        {
            let _ = fs::remove_file(&temporary);
        }
    }
    result
}

// 作成済みpathをfile typeに応じて削除する
fn remove_created_path(path: &Path, identity: PathIdentity, recursive: bool) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    if !identity.matches(&metadata) {
        bail!("created path identity changed: {}", path.display());
    }
    if metadata.file_type().is_dir() && recursive {
        fs::remove_dir_all(path)?;
    } else if metadata.file_type().is_dir() {
        fs::remove_dir(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

// move先を元pathへ戻し、既存の元pathは上書きしない
fn restore_moved_path(from: &Path, to: &Path) -> Result<()> {
    move_without_replace(to, from)
}

/// 完成済みtemporary regular fileをhard linkで上書きせずpublishする
pub(crate) fn publish_initial_file(
    temporary: &Path,
    destination: &Path,
    identity: PathIdentity,
) -> Result<InitialPublish> {
    publish_initial_file_with(temporary, destination, identity, |source, destination| {
        let source = path_c_string(source, "source")?;
        let destination = path_c_string(destination, "destination")?;
        // SAFETY: 両pathはNUL終端済みでcall完了まで有効なpointerを保持する
        let result = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error().into())
        }
    })
}

fn publish_initial_file_with(
    temporary: &Path,
    destination: &Path,
    identity: PathIdentity,
    link: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<InitialPublish> {
    if PathIdentity::from_path(temporary)? != identity {
        bail!("temporary file identity changed: {}", temporary.display());
    }
    let link_result = link(temporary, destination);
    let destination_identity = path_identity(destination)?;
    if destination_identity != Some(identity) {
        let error = match destination_identity {
            None => link_result
                .err()
                .unwrap_or_else(|| anyhow!("published destination is missing")),
            Some(_) => anyhow!("published destination has a different identity"),
        };
        let _ = remove_if_identity(temporary, identity);
        return Err(error);
    }

    let cleanup_error = remove_if_identity(temporary, identity)
        .err()
        .map(|error| error.to_string());
    Ok(InitialPublish { cleanup_error })
}

/// destinationを上書きせずmoveし、unsupported filesystemでは通常renameへfallbackする
pub(crate) fn move_without_replace(source: &Path, destination: &Path) -> Result<()> {
    move_without_replace_with(
        source,
        destination,
        rename_no_replace,
        |source, destination| fs::rename(source, destination).map_err(Into::into),
    )
}

fn move_without_replace_with(
    source: &Path,
    destination: &Path,
    rename_no_replace: impl FnOnce(&Path, &Path) -> Result<()>,
    rename_fallback: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<()> {
    let identity = PathIdentity::from_path(source)?;
    ensure_missing(destination)?;
    let primary = rename_no_replace(source, destination);
    match move_state(source, destination, identity)? {
        MoveState::Moved => return Ok(()),
        MoveState::Unchanged => {}
        MoveState::Unknown => bail!("move result has an unexpected filesystem state"),
    }
    let error = match primary {
        Ok(()) => bail!("move reported success without changing filesystem state"),
        Err(error) => error,
    };
    if error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::raw_os_error)
        != Some(libc::EINVAL)
    {
        return Err(error);
    }

    if PathIdentity::from_path(source)? != identity {
        bail!("move source identity changed: {}", source.display());
    }
    ensure_missing(destination)?;
    let fallback = rename_fallback(source, destination);
    match move_state(source, destination, identity)? {
        MoveState::Moved => Ok(()),
        MoveState::Unchanged => Err(fallback
            .err()
            .unwrap_or_else(|| anyhow!("fallback rename did not move source"))),
        MoveState::Unknown => bail!("fallback rename result has an unexpected filesystem state"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveState {
    Moved,
    Unchanged,
    Unknown,
}

fn move_state(source: &Path, destination: &Path, identity: PathIdentity) -> Result<MoveState> {
    let source = path_identity(source)?;
    let destination = path_identity(destination)?;
    Ok(match (source, destination) {
        (None, Some(current)) if current == identity => MoveState::Moved,
        (Some(current), None) if current == identity => MoveState::Unchanged,
        _ => MoveState::Unknown,
    })
}

fn ensure_missing(path: &Path) -> Result<()> {
    if path_identity(path)?.is_some() {
        bail!("move destination already exists: {}", path.display());
    }
    Ok(())
}

fn path_identity(path: &Path) -> Result<Option<PathIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(PathIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_if_identity(path: &Path, identity: PathIdentity) -> Result<()> {
    match path_identity(path)? {
        None => Ok(()),
        Some(current) if current == identity => fs::remove_file(path).map_err(Into::into),
        Some(_) => bail!("temporary file identity changed: {}", path.display()),
    }
}

fn rename_no_replace(source: &Path, destination: &Path) -> Result<()> {
    let source = path_c_string(source, "source")?;
    let destination = path_c_string(destination, "destination")?;

    // SAFETY: 両pathはNUL終端済みで、有効なpointerをcall完了まで保持する
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}

fn path_c_string(path: &Path, label: &str) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| anyhow!("{label} path contains a NUL byte"))
}

// Stow済みpackageを固定したsource / target rootから解除する
fn unstow_package(package: &str, executable: &Path, dir: &Path, target: &Path) -> Result<()> {
    let status = build_unstow_command(package, executable, dir, target).status()?;
    if !status.success() {
        bail!("stow --delete failed for package {package}: {status}");
    }
    Ok(())
}

// working directory非依存のStow delete commandを構成する
fn build_unstow_command(package: &str, executable: &Path, dir: &Path, target: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--delete")
        .arg("--target")
        .arg(target)
        .arg("--dir")
        .arg(dir)
        .arg(package)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    command
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::env;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStringExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    // test pathをidentity付きCreated Actionへ変換する
    fn created_action(path: &Path) -> Result<Action> {
        Ok(Action::Created {
            path: path.to_path_buf(),
            identity: PathIdentity::from_path(path)?,
            recursive: false,
        })
    }

    // test用のabsolute Stow contextを持つActionを作成する
    fn stowed_action(package: &str) -> Action {
        Action::Stowed {
            package: package.to_owned(),
            executable: PathBuf::from("/opt/pixi/bin/stow"),
            dir: PathBuf::from("/home/tester/.dotfiles"),
            target: PathBuf::from("/home/tester"),
        }
    }

    // 並列test間でtemporary directory名が衝突しないための連番
    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    // transaction test専用directoryの作成とcleanupを所有するfixture
    struct TestDirectory {
        // fixtureが所有するtemporary directory path
        path: PathBuf,
    }

    impl TestDirectory {
        // process IDと連番から衝突しないtest directoryを作成する
        fn new() -> Result<Self> {
            for _ in 0..128 {
                let sequence = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = env::temp_dir().join(format!(
                    "eiyah-transaction-test-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Ok(Self { path }),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            bail!("failed to allocate transaction test directory");
        }
    }

    impl Drop for TestDirectory {
        // test成否にかかわらずfixture配下だけを可能な限りcleanupする
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    // Created actionが作成済みpathを削除することを検証する
    fn created_action_rolls_back_created_path() -> Result<()> {
        let directory = TestDirectory::new()?;
        let created = directory.path.join("created");
        fs::write(&created, b"created")?;

        let mut transaction = Transaction::new();
        transaction.record(created_action(&created)?);
        transaction.rollback()?;

        assert!(!created.exists());
        Ok(())
    }

    #[test]
    // Created pathが他者所有inodeへ置換された場合は削除しない
    fn created_action_rejects_identity_mismatch() -> Result<()> {
        let directory = TestDirectory::new()?;
        let created = directory.path.join("created");
        fs::write(&created, b"owned")?;
        let action = created_action(&created)?;
        let replacement = directory.path.join("replacement");
        fs::write(&replacement, b"replacement")?;
        fs::rename(&replacement, &created)?;

        let mut transaction = Transaction::new();
        transaction.record(action);
        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read(&created)?, b"replacement");
        Ok(())
    }

    #[test]
    // Moved actionがmove先を元pathへ復元することを検証する
    fn moved_action_restores_original_path() -> Result<()> {
        let directory = TestDirectory::new()?;
        let original = directory.path.join("original");
        let moved = directory.path.join("moved");
        fs::write(&original, b"original")?;
        fs::rename(&original, &moved)?;

        let mut transaction = Transaction::new();
        transaction.record(Action::Moved {
            from: original.clone(),
            to: moved.clone(),
        });
        transaction.rollback()?;

        assert_eq!(fs::read(&original)?, b"original");
        assert!(!moved.exists());
        Ok(())
    }

    #[test]
    // backup indexがnon-UTF-8 path bytesをlosslessにencode・decodeする
    fn backup_index_round_trips_relative_path_bytes() -> Result<()> {
        let relative = PathBuf::from(OsString::from_vec(b".config/\xff-file".to_vec()));
        let encoded = encode_backup_index_entry(&relative)?;
        assert!(
            encoded
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
        assert_eq!(decode_backup_index_entry(&encoded)?, relative);
        for invalid in [Path::new(""), Path::new("/absolute"), Path::new("a/../b")] {
            assert!(encode_backup_index_entry(invalid).is_err());
        }
        assert!(decode_backup_index_entry(b"ABCDEF").is_err());
        assert!(decode_backup_index_entry(b"0").is_err());
        assert!(decode_backup_index_entry(b"7a00").is_err());
        Ok(())
    }

    #[test]
    // commit前に置換された他者所有temporary fileをrenameもcleanupもしない
    fn preserves_replaced_backup_index_temporary_file() -> Result<()> {
        let directory = TestDirectory::new()?;
        let index = directory.path.join("index");
        let entry = encode_backup_index_entry(Path::new(".cshrc"))?;
        let mut replacement = None;

        assert!(
            write_backup_index_with(&index, &[entry], |temporary| {
                fs::remove_file(temporary)?;
                fs::write(temporary, b"concurrent temporary")?;
                replacement = Some(temporary.to_path_buf());
                Ok(())
            })
            .is_err()
        );

        let replacement = replacement.unwrap();
        assert_eq!(fs::read(&replacement)?, b"concurrent temporary");
        assert!(!index.exists());
        Ok(())
    }

    #[test]
    // BackedUp rollbackがHOMEを復元して対応index entryだけを削除する
    fn backed_up_action_restores_path_and_updates_index() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home_path = directory.path.join("home/.cshrc");
        let backup = directory.path.join("state/backup/home/.cshrc");
        let index = directory.path.join("state/backup/index");
        fs::create_dir_all(backup.parent().unwrap())?;
        fs::create_dir_all(home_path.parent().unwrap())?;
        fs::write(&backup, b"original")?;
        let entry = encode_backup_index_entry(Path::new(".cshrc"))?;
        let other = encode_backup_index_entry(Path::new(".config/other"))?;
        write_backup_index(&index, &[entry.clone(), other.clone()])?;
        assert_eq!(fs::metadata(&index)?.permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&index, fs::Permissions::from_mode(0o644))?;

        let mut transaction = Transaction::new();
        transaction.record(Action::BackedUp {
            from: home_path.clone(),
            to: backup.clone(),
            index: index.clone(),
            entry,
        });
        transaction.rollback()?;

        assert_eq!(fs::read(home_path)?, b"original");
        assert!(!backup.exists());
        assert_eq!(read_backup_index(&index)?, Some(vec![other]));
        assert_eq!(fs::metadata(&index)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    // BackedUp restore失敗時はbackup sourceとindex entryを保持する
    fn backed_up_rollback_preserves_state_on_restore_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home_path = directory.path.join("home/.cshrc");
        let backup = directory.path.join("state/backup/home/.cshrc");
        let index = directory.path.join("state/backup/index");
        fs::create_dir_all(backup.parent().unwrap())?;
        fs::create_dir_all(home_path.parent().unwrap())?;
        fs::write(&home_path, b"collision")?;
        fs::write(&backup, b"original")?;
        let entry = encode_backup_index_entry(Path::new(".cshrc"))?;
        write_backup_index(&index, std::slice::from_ref(&entry))?;

        assert!(rollback_backup(&home_path, &backup, &index, &entry).is_err());
        assert_eq!(fs::read(&home_path)?, b"collision");
        assert_eq!(fs::read(&backup)?, b"original");
        assert_eq!(read_backup_index(&index)?, Some(vec![entry]));
        Ok(())
    }

    #[test]
    // index更新failure後もHOMEへ復元済みpathをbackupへ戻さない
    fn backed_up_rollback_keeps_restored_path_after_index_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home_path = directory.path.join("home/.cshrc");
        let backup = directory.path.join("state/backup/home/.cshrc");
        let index = directory.path.join("state/backup/index");
        fs::create_dir_all(backup.parent().unwrap())?;
        fs::create_dir_all(home_path.parent().unwrap())?;
        fs::write(&backup, b"original")?;
        let entry = encode_backup_index_entry(Path::new(".cshrc"))?;
        write_backup_index(&index, std::slice::from_ref(&entry))?;

        assert!(
            rollback_backup_with(
                &home_path,
                &backup,
                &index,
                &entry,
                |_| Ok(()),
                |_, _| Err(anyhow!("injected index failure")),
            )
            .is_err()
        );
        assert_eq!(fs::read(&home_path)?, b"original");
        assert!(!backup.exists());
        assert_eq!(read_backup_index(&index)?, Some(vec![entry]));
        Ok(())
    }

    #[test]
    // BackedUp source replacement raceで他者所有pathを移動しない
    fn backed_up_rollback_rejects_source_identity_change() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home_path = directory.path.join("home/.cshrc");
        let backup = directory.path.join("state/backup/home/.cshrc");
        let original = directory.path.join("original");
        let index = directory.path.join("state/backup/index");
        fs::create_dir_all(backup.parent().unwrap())?;
        fs::create_dir_all(home_path.parent().unwrap())?;
        fs::write(&backup, b"original")?;
        let entry = encode_backup_index_entry(Path::new(".cshrc"))?;
        write_backup_index(&index, std::slice::from_ref(&entry))?;

        assert!(
            rollback_backup_with(
                &home_path,
                &backup,
                &index,
                &entry,
                |source| {
                    fs::rename(source, &original)?;
                    fs::write(source, b"other owner")?;
                    Ok(())
                },
                |_, _| unreachable!(),
            )
            .is_err()
        );
        assert!(!home_path.exists());
        assert_eq!(fs::read(&backup)?, b"other owner");
        assert_eq!(fs::read(&original)?, b"original");
        assert_eq!(read_backup_index(&index)?, Some(vec![entry]));
        Ok(())
    }

    #[test]
    // Stowed actionがpackageを明示rootでunstowすることを検証する
    fn stowed_action_uses_explicit_source_and_target_roots() -> Result<()> {
        let mut transaction = Transaction::new();
        transaction.record(stowed_action("zsh"));
        let mut packages = Vec::new();
        transaction.rollback_with(|package, _, _, _| {
            packages.push(package.to_owned());
            Ok(())
        })?;
        assert_eq!(packages, ["zsh"]);

        let command = build_unstow_command(
            "zsh",
            Path::new("/opt/pixi/bin/stow"),
            Path::new("/home/tester/.dotfiles"),
            Path::new("/home/tester"),
        );
        let arguments: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(command.get_program(), OsStr::new("/opt/pixi/bin/stow"));
        assert_eq!(
            arguments,
            [
                "--delete",
                "--target",
                "/home/tester",
                "--dir",
                "/home/tester/.dotfiles",
                "zsh",
            ]
            .map(OsString::from)
        );
        Ok(())
    }

    #[test]
    // rollbackがActionを記録と逆の順序で処理することを検証する
    fn rollback_uses_reverse_action_order() -> Result<()> {
        let mut transaction = Transaction::new();
        for package in ["first", "second", "third"] {
            transaction.record(stowed_action(package));
        }
        let mut packages = Vec::new();
        transaction.rollback_with(|package, _, _, _| {
            packages.push(package.to_owned());
            Ok(())
        })?;

        assert_eq!(packages, ["third", "second", "first"]);
        Ok(())
    }

    #[test]
    // 失敗したoperationを記録しなければrollback対象にならないことを検証する
    fn failed_operation_is_not_recorded() -> Result<()> {
        let directory = TestDirectory::new()?;
        let missing = directory.path.join("missing");
        let destination = directory.path.join("destination");
        let operation = fs::rename(&missing, &destination);
        assert!(operation.is_err());

        let mut transaction = Transaction::new();
        transaction.rollback()?;
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    // Moved rollbackが既存の元pathを上書きしないことを検証する
    fn moved_rollback_rejects_backup_overwrite() -> Result<()> {
        let directory = TestDirectory::new()?;
        let original = directory.path.join("original");
        let moved = directory.path.join("moved");
        fs::write(&original, b"new")?;
        fs::write(&moved, b"backup")?;

        let mut transaction = Transaction::new();
        transaction.record(Action::Moved {
            from: original.clone(),
            to: moved.clone(),
        });
        assert!(transaction.rollback().is_err());
        assert_eq!(fs::read(&original)?, b"new");
        assert_eq!(fs::read(&moved)?, b"backup");
        Ok(())
    }

    #[test]
    // atomic renameがdestination競合時に両pathを維持することを検証する
    fn atomic_rename_rejects_destination_conflict() -> Result<()> {
        let directory = TestDirectory::new()?;
        let source = directory.path.join("source");
        let destination = directory.path.join("destination");
        fs::write(&source, b"source")?;
        fs::write(&destination, b"destination")?;

        assert!(move_without_replace(&source, &destination).is_err());
        assert_eq!(fs::read(&source)?, b"source");
        assert_eq!(fs::read(&destination)?, b"destination");
        Ok(())
    }

    #[test]
    // NFSでlink応答が曖昧でもdestination identity一致をpublish成功とする
    fn initial_publish_accepts_ambiguous_link_success() -> Result<()> {
        let directory = TestDirectory::new()?;
        let temporary = directory.path.join("temporary");
        let destination = directory.path.join("destination");
        fs::write(&temporary, b"complete")?;
        let identity = PathIdentity::from_path(&temporary)?;

        let published = publish_initial_file_with(
            &temporary,
            &destination,
            identity,
            |source, destination| {
                fs::hard_link(source, destination)?;
                Err(io::Error::from_raw_os_error(libc::EIO).into())
            },
        )?;

        assert_eq!(published.cleanup_error, None);
        assert_eq!(fs::read(&destination)?, b"complete");
        assert!(!temporary.exists());
        Ok(())
    }

    #[test]
    // initial publish collisionでexisting destinationを維持する
    fn initial_publish_rejects_destination_collision() -> Result<()> {
        let directory = TestDirectory::new()?;
        let temporary = directory.path.join("temporary");
        let destination = directory.path.join("destination");
        fs::write(&temporary, b"ours")?;
        fs::write(&destination, b"other")?;
        let identity = PathIdentity::from_path(&temporary)?;

        assert!(publish_initial_file(&temporary, &destination, identity).is_err());
        assert_eq!(fs::read(&destination)?, b"other");
        assert!(!temporary.exists());
        Ok(())
    }

    #[test]
    // publish後のtemporary replacementを削除せずpublish成功を維持する
    fn initial_publish_preserves_replaced_temporary_after_success() -> Result<()> {
        let directory = TestDirectory::new()?;
        let temporary = directory.path.join("temporary");
        let original = directory.path.join("original");
        let destination = directory.path.join("destination");
        fs::write(&temporary, b"complete")?;
        let identity = PathIdentity::from_path(&temporary)?;

        let published = publish_initial_file_with(
            &temporary,
            &destination,
            identity,
            |source, destination| {
                fs::hard_link(source, destination)?;
                fs::rename(source, &original)?;
                fs::write(source, b"replacement")?;
                Ok(())
            },
        )?;

        assert!(published.cleanup_error.is_some());
        assert_eq!(fs::read(&destination)?, b"complete");
        assert_eq!(fs::read(&temporary)?, b"replacement");
        assert_eq!(fs::read(&original)?, b"complete");
        Ok(())
    }

    #[test]
    // RENAME_NOREPLACE非対応時に通常renameへfallbackする
    fn move_falls_back_after_unsupported_rename() -> Result<()> {
        let directory = TestDirectory::new()?;
        let source = directory.path.join("source");
        let destination = directory.path.join("destination");
        fs::write(&source, b"source")?;

        move_without_replace_with(
            &source,
            &destination,
            |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
            |source, destination| fs::rename(source, destination).map_err(Into::into),
        )?;

        assert!(!source.exists());
        assert_eq!(fs::read(&destination)?, b"source");
        Ok(())
    }

    #[test]
    // 通常rename fallbackがregular file以外のpath typeもそのままmoveする
    fn move_fallback_supports_arbitrary_path_types() -> Result<()> {
        let directory = TestDirectory::new()?;

        let source_directory = directory.path.join("source-directory");
        let destination_directory = directory.path.join("destination-directory");
        fs::create_dir(&source_directory)?;
        fs::write(source_directory.join("content"), b"directory")?;
        move_without_replace_with(
            &source_directory,
            &destination_directory,
            |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
            |source, destination| fs::rename(source, destination).map_err(Into::into),
        )?;
        assert_eq!(
            fs::read(destination_directory.join("content"))?,
            b"directory"
        );

        let source_symlink = directory.path.join("source-symlink");
        let destination_symlink = directory.path.join("destination-symlink");
        std::os::unix::fs::symlink("target", &source_symlink)?;
        move_without_replace_with(
            &source_symlink,
            &destination_symlink,
            |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
            |source, destination| fs::rename(source, destination).map_err(Into::into),
        )?;
        assert_eq!(
            fs::read_link(&destination_symlink)?,
            PathBuf::from("target")
        );
        Ok(())
    }

    #[test]
    // fallbackの曖昧なerrorをdestination identityから成功と判定する
    fn move_accepts_ambiguous_fallback_success() -> Result<()> {
        let directory = TestDirectory::new()?;
        let source = directory.path.join("source");
        let destination = directory.path.join("destination");
        fs::write(&source, b"source")?;

        move_without_replace_with(
            &source,
            &destination,
            |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
            |source, destination| {
                fs::rename(source, destination)?;
                Err(io::Error::from_raw_os_error(libc::EIO).into())
            },
        )?;

        assert!(!source.exists());
        assert_eq!(fs::read(&destination)?, b"source");
        Ok(())
    }

    #[test]
    // fallback未実行と予期しないstateをfailureとして保持する
    fn move_rejects_failed_and_unknown_fallback_states() -> Result<()> {
        let directory = TestDirectory::new()?;
        let source = directory.path.join("source");
        let destination = directory.path.join("destination");
        fs::write(&source, b"source")?;
        assert!(
            move_without_replace_with(
                &source,
                &destination,
                |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
                |_, _| Err(io::Error::from_raw_os_error(libc::EIO).into()),
            )
            .is_err()
        );
        assert_eq!(fs::read(&source)?, b"source");
        assert!(!destination.exists());

        assert!(
            move_without_replace_with(
                &source,
                &destination,
                |_, _| Err(io::Error::from_raw_os_error(libc::EINVAL).into()),
                |source, destination| {
                    fs::rename(source, destination)?;
                    fs::write(source, b"replacement")?;
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(fs::read(&source)?, b"replacement");
        assert_eq!(fs::read(&destination)?, b"source");
        Ok(())
    }

    #[test]
    // rollback失敗後も残りのActionを可能な限りundoすることを検証する
    fn rollback_continues_after_an_undo_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let created = directory.path.join("created");
        fs::write(&created, b"created")?;

        let mut transaction = Transaction::new();
        transaction.record(created_action(&created)?);
        transaction.record(Action::Moved {
            from: directory.path.join("original"),
            to: directory.path.join("missing"),
        });
        assert!(transaction.rollback().is_err());
        assert!(!created.exists());
        Ok(())
    }

    #[test]
    // LockGuardが仕様通りのpathへlock fileを作成することを検証する
    fn lock_guard_acquires_expected_lock() -> Result<()> {
        let directory = TestDirectory::new()?;
        let _guard = LockGuard::acquire(&directory.path)?;
        assert!(directory.path.join("eiyah/lock").is_file());
        Ok(())
    }

    #[test]
    // 同一lockへの重複したnon-blocking取得を拒否することを検証する
    fn lock_guard_rejects_duplicate_lock() -> Result<()> {
        let directory = TestDirectory::new()?;
        let guard = LockGuard::acquire(&directory.path)?;
        assert!(LockGuard::acquire(&directory.path).is_err());
        drop(guard);
        let _reacquired = LockGuard::acquire(&directory.path)?;
        Ok(())
    }

    #[test]
    // commit後のActionがrollbackされないことを検証する
    fn committed_action_is_not_rolled_back() -> Result<()> {
        let directory = TestDirectory::new()?;
        let created = directory.path.join("created");
        fs::write(&created, b"created")?;

        let mut transaction = Transaction::new();
        transaction.record(created_action(&created)?);
        transaction.commit();
        transaction.rollback()?;

        assert!(created.is_file());
        Ok(())
    }
}
