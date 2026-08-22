// ==================================================
// @file src/transaction.rs
// @brief Transaction action tracking and rollback
// ==================================================

use std::env;
use std::fs;
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
    /// rollback対象となる成功済みaction
    actions: Vec<Action>,
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
        // 失敗後も残りのundoを継続するため、各errorを最後まで保持する
        let mut errors = Vec::new();

        for action in self.actions.drain(..).rev() {
            if let Err(error) = undo(action) {
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

/// Action variantごとのfilesystemまたはStow変更を取り消す
fn undo(action: Action) -> Result<()> {
    match action {
        Action::Created(path) => remove_created_path(&path),
        Action::Moved { from, to } => restore_moved_path(&from, &to),
        Action::Stowed(package) => unstow_package(&package),
    }
}

/// 作成済みpathをfile typeに応じて削除する
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

/// move先を元pathへ戻し、既存の元pathは上書きしない
fn restore_moved_path(from: &Path, to: &Path) -> Result<()> {
    match fs::symlink_metadata(from) {
        Ok(_) => bail!("rollback destination already exists: {}", from.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    fs::rename(to, from)?;
    Ok(())
}

/// Stow済みpackageを固定したsource / target rootから解除する
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

    // install済みPrivate dotfilesを保持するStow source root
    let source_root = home.join(".dotfiles");
    let status = Command::new("stow")
        .arg("--delete")
        .arg("--dir")
        .arg(&source_root)
        .arg("--target")
        .arg(&home)
        .arg(package)
        .status()?;
    if !status.success() {
        bail!("stow --delete failed for package {package}: {status}");
    }
    Ok(())
}
