// ==================================================
// @file src/transaction.rs
// @brief Transaction action tracking and rollback
// ==================================================

use std::env;
use std::ffi::CString;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, anyhow, bail};

/// rollback対象として記録する成功済みoperation
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// transaction中に新規作成したpath
    Created(PathBuf),
    /// transaction中に移動したpathと移動先
    Moved {
        /// move前のpath
        from: PathBuf,
        /// move後のpath
        to: PathBuf,
    },
    /// transaction中にStowしたpackage名
    Stowed(String),
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
    fn rollback_with(&mut self, mut unstow: impl FnMut(&str) -> Result<()>) -> Result<()> {
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
fn undo(action: Action, unstow: &mut impl FnMut(&str) -> Result<()>) -> Result<()> {
    match action {
        Action::Created(path) => remove_created_path(&path),
        Action::Moved { from, to } => restore_moved_path(&from, &to),
        Action::Stowed(package) => unstow(&package),
    }
}

// 作成済みpathをfile typeに応じて削除する
fn remove_created_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

// move先を元pathへ戻し、既存の元pathは上書きしない
fn restore_moved_path(from: &Path, to: &Path) -> Result<()> {
    rename_without_replace(to, from)
}

// destinationの存在確認とmoveをatomicに行い上書きを拒否する
fn rename_without_replace(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| anyhow!("source path contains a NUL byte"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| anyhow!("destination path contains a NUL byte"))?;

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
    if result != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

// Stow済みpackageを固定したsource / target rootから解除する
fn unstow_package(package: &str) -> Result<()> {
    // working directoryへ依存しないStow targetの基準path
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))?;
    if home.as_os_str().is_empty() {
        bail!("HOME must not be empty");
    }
    if !home.is_absolute() {
        bail!("HOME must be an absolute path: {}", home.display());
    }

    let status = build_unstow_command(package, &home).status()?;
    if !status.success() {
        bail!("stow --delete failed for package {package}: {status}");
    }
    Ok(())
}

// working directory非依存のStow delete commandを構成する
fn build_unstow_command(package: &str, home: &Path) -> Command {
    // install済みPrivate dotfilesを保持するStow source root
    let source_root = home.join(".dotfiles");
    let mut command = Command::new("stow");
    command
        .arg("--delete")
        .arg("--dir")
        .arg(&source_root)
        .arg("--target")
        .arg(&home)
        .arg(package);
    command
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

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
        transaction.record(Action::Created(created.clone()));
        transaction.rollback()?;

        assert!(!created.exists());
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
    // Stowed actionがpackageを明示rootでunstowすることを検証する
    fn stowed_action_uses_explicit_source_and_target_roots() -> Result<()> {
        let mut transaction = Transaction::new();
        transaction.record(Action::Stowed("zsh".to_owned()));
        let mut packages = Vec::new();
        transaction.rollback_with(|package| {
            packages.push(package.to_owned());
            Ok(())
        })?;
        assert_eq!(packages, ["zsh"]);

        let command = build_unstow_command("zsh", Path::new("/home/tester"));
        let arguments: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(command.get_program(), OsStr::new("stow"));
        assert_eq!(
            arguments,
            [
                "--delete",
                "--dir",
                "/home/tester/.dotfiles",
                "--target",
                "/home/tester",
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
            transaction.record(Action::Stowed(package.to_owned()));
        }
        let mut packages = Vec::new();
        transaction.rollback_with(|package| {
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

        assert!(rename_without_replace(&source, &destination).is_err());
        assert_eq!(fs::read(&source)?, b"source");
        assert_eq!(fs::read(&destination)?, b"destination");
        Ok(())
    }

    #[test]
    // rollback失敗後も残りのActionを可能な限りundoすることを検証する
    fn rollback_continues_after_an_undo_failure() -> Result<()> {
        let directory = TestDirectory::new()?;
        let created = directory.path.join("created");
        fs::write(&created, b"created")?;

        let mut transaction = Transaction::new();
        transaction.record(Action::Created(created.clone()));
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
        transaction.record(Action::Created(created.clone()));
        transaction.commit();
        transaction.rollback()?;

        assert!(created.is_file());
        Ok(())
    }
}
