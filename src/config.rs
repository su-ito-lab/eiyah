// ==================================================
// @file src/config.rs
// @brief Configuration and installation path handling
// ==================================================

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// user設定を保存するTOML file名
const CONFIG_FILE_NAME: &str = "config.toml";
/// 各XDG base directory配下で使用するapplication directory名
const EIYAH_DIRECTORY_NAME: &str = "eiyah";
/// install後の配置情報を保存するTOML file名
const INSTALL_METADATA_FILE_NAME: &str = "install.toml";
/// atomic saveのtemporary fileへ設定するpermission
const TEMP_FILE_MODE: u32 = 0o600;

/// process内でtemporary file名の重複を避けるための連番
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

// --------------------------------------------------
// Models
// --------------------------------------------------

#[derive(Clone, Debug, Eq, PartialEq)]
/// 初回解決またはinstall metadataから復元したEiyahの配置path一式
pub struct ResolvedPaths {
    /// user設定のXDG base directory
    pub config_home: PathBuf,
    /// application dataのXDG base directory
    pub data_home: PathBuf,
    /// runtime stateのXDG base directory
    pub state_home: PathBuf,
    /// 再生成可能dataのXDG base directory
    pub cache_home: PathBuf,
    /// Eiyah本体とinstall metadataを配置するdirectory
    pub eiyah_prefix: PathBuf,
    /// user設定TOMLの配置path
    pub eiyah_config: PathBuf,
    /// Eiyah専用Pixi homeの配置path
    pub pixi_home: PathBuf,
}

impl ResolvedPaths {
    /// 永続化された4つのXDG base pathから配置path一式を復元する
    pub fn from_install_metadata(metadata: InstallMetadata) -> Result<Self> {
        metadata.validate()?;
        Ok(Self::from_homes(
            metadata.config_home,
            metadata.data_home,
            metadata.state_home,
            metadata.cache_home,
        ))
    }

    /// 検証済みのXDG base pathへEiyah固有のrelative pathを付加する
    fn from_homes(
        config_home: PathBuf,
        data_home: PathBuf,
        state_home: PathBuf,
        cache_home: PathBuf,
    ) -> Self {
        // install metadataとbinaryを共有するEiyah data root
        let eiyah_prefix = data_home.join(EIYAH_DIRECTORY_NAME);
        // public commandが参照するuser設定file
        let eiyah_config = config_home
            .join(EIYAH_DIRECTORY_NAME)
            .join(CONFIG_FILE_NAME);
        // Public Eiyahが管理するPixi専用領域
        let pixi_home = data_home.join(EIYAH_DIRECTORY_NAME).join("pixi");

        Self {
            config_home,
            data_home,
            state_home,
            cache_home,
            eiyah_prefix,
            eiyah_config,
            pixi_home,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
/// userが変更可能なEiyah設定
pub struct Config {
    /// shell handoff前にCAD server statusを表示するかを示すflag
    pub show_cad_status: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
/// install後も現在のenvironmentに依存せず配置を復元するためのmetadata
pub struct InstallMetadata {
    /// install時に解決したconfig base directory
    pub config_home: PathBuf,
    /// install時に解決したdata base directory
    pub data_home: PathBuf,
    /// install時に解決したstate base directory
    pub state_home: PathBuf,
    /// install時に解決したcache base directory
    pub cache_home: PathBuf,
}

impl InstallMetadata {
    /// metadataの全pathが空でないabsolute pathであることを保証する
    fn validate(&self) -> Result<()> {
        validate_absolute_path("config-home", &self.config_home)?;
        validate_absolute_path("data-home", &self.data_home)?;
        validate_absolute_path("state-home", &self.state_home)?;
        validate_absolute_path("cache-home", &self.cache_home)?;
        Ok(())
    }
}

impl From<&ResolvedPaths> for InstallMetadata {
    /// derived pathを除外し、正本となる4つのXDG base pathだけを抽出する
    fn from(paths: &ResolvedPaths) -> Self {
        Self {
            config_home: paths.config_home.clone(),
            data_home: paths.data_home.clone(),
            state_home: paths.state_home.clone(),
            cache_home: paths.cache_home.clone(),
        }
    }
}

// --------------------------------------------------
// Path Resolution
// --------------------------------------------------

/// 初回install向けにprocess environmentからXDG配置pathを解決する
pub fn resolve_paths() -> Result<ResolvedPaths> {
    resolve_paths_from(|name| env::var_os(name))
}

/// environment取得元を差し替え可能にしてXDG fallback規則を一元化する
fn resolve_paths_from(
    mut get_environment: impl FnMut(&str) -> Option<OsString>,
) -> Result<ResolvedPaths> {
    // 全fallbackの基準となるため、XDG値に関係なくHOMEを先に検証する
    let home = required_absolute_environment_path("HOME", get_environment("HOME"))?;
    let config_home = xdg_path(
        "XDG_CONFIG_HOME",
        get_environment("XDG_CONFIG_HOME"),
        &home,
        ".config",
    )?;
    let data_home = xdg_path(
        "XDG_DATA_HOME",
        get_environment("XDG_DATA_HOME"),
        &home,
        ".local/share",
    )?;
    let state_home = xdg_path(
        "XDG_STATE_HOME",
        get_environment("XDG_STATE_HOME"),
        &home,
        ".local/state",
    )?;
    let cache_home = xdg_path(
        "XDG_CACHE_HOME",
        get_environment("XDG_CACHE_HOME"),
        &home,
        ".cache",
    )?;

    Ok(ResolvedPaths::from_homes(
        config_home,
        data_home,
        state_home,
        cache_home,
    ))
}

/// fallbackできない必須environment pathへ共通の絶対path制約を適用する
fn required_absolute_environment_path(name: &str, value: Option<OsString>) -> Result<PathBuf> {
    let value = value.with_context(|| format!("{name} is not set"))?;
    let path = PathBuf::from(value);
    validate_absolute_path(name, &path)?;
    Ok(path)
}

/// XDG値が未設定または空の場合だけHOME配下のfallbackを選択する
fn xdg_path(name: &str, value: Option<OsString>, home: &Path, fallback: &str) -> Result<PathBuf> {
    match value {
        None => Ok(home.join(fallback)),
        Some(value) if value.is_empty() => Ok(home.join(fallback)),
        Some(value) => {
            let path = PathBuf::from(value);
            validate_absolute_path(name, &path)?;
            Ok(path)
        }
    }
}

/// filesystem操作がcurrent directoryへ依存しないようpath制約を検証する
fn validate_absolute_path(name: &str, path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("{name} must not be empty");
    }
    if !path.is_absolute() {
        bail!("{name} must be an absolute path: {}", path.display());
    }
    Ok(())
}

/// public symlinkのabsolute targetから、存在確認なしでmetadata pathを導出する
pub fn discover_install_metadata(public_entry_point: &Path) -> Result<PathBuf> {
    // broken symlinkも扱うため、targetのcanonicalizeや存在確認は行わない
    let target = fs::read_link(public_entry_point).with_context(|| {
        format!(
            "failed to read public entry point symlink: {}",
            public_entry_point.display()
        )
    })?;

    if !target.is_absolute() {
        bail!(
            "public entry point symlink target must be absolute: {}",
            target.display()
        );
    }

    // targetが契約上のbin/eiyah形状を持つか検証するための親directory
    let binary_directory = target.parent().with_context(|| {
        format!(
            "public entry point target has no parent: {}",
            target.display()
        )
    })?;
    if target.file_name() != Some(OsStr::new("eiyah"))
        || binary_directory.file_name() != Some(OsStr::new("bin"))
    {
        bail!(
            "public entry point target must end with bin/eiyah: {}",
            target.display()
        );
    }

    // binの親をinstall metadataの正本が置かれるprefixとして扱う
    let eiyah_prefix = binary_directory.parent().with_context(|| {
        format!(
            "public entry point target has no Eiyah prefix: {}",
            target.display()
        )
    })?;

    Ok(eiyah_prefix.join(INSTALL_METADATA_FILE_NAME))
}

// --------------------------------------------------
// TOML Storage
// --------------------------------------------------

/// user設定fileを読み、厳格なschemaでConfigへ変換する
pub fn load_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("failed to parse config: {}", path.display()))
}

/// ConfigをTOML化し、既存fileを安全にatomic replacementする
pub fn save_config(path: &Path, config: &Config) -> Result<()> {
    let contents = toml::to_string(config).context("failed to serialize config")?;
    atomic_save(path, contents.as_bytes())
}

/// install metadataを読み、TOML schemaとpath制約だけを検証する
pub fn load_install_metadata(path: &Path) -> Result<InstallMetadata> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read installation metadata: {}", path.display()))?;
    let metadata: InstallMetadata = toml::from_str(&contents)
        .with_context(|| format!("failed to parse installation metadata: {}", path.display()))?;
    metadata.validate()?;
    Ok(metadata)
}

/// resolved pathから4つのbase pathだけをmetadataとしてatomic保存する
pub fn save_install_metadata(paths: &ResolvedPaths) -> Result<()> {
    let metadata = InstallMetadata::from(paths);
    metadata.validate()?;
    let contents =
        toml::to_string(&metadata).context("failed to serialize installation metadata")?;
    atomic_save(
        &paths.eiyah_prefix.join(INSTALL_METADATA_FILE_NAME),
        contents.as_bytes(),
    )
}

/// 同一directoryのtemporary fileを介して、通常fileだけをatomic置換する
fn atomic_save(target: &Path, contents: &[u8]) -> Result<()> {
    validate_replace_target(target)?;

    let parent = target
        .parent()
        .with_context(|| format!("target has no parent directory: {}", target.display()))?;
    let file_name = target
        .file_name()
        .with_context(|| format!("target has no file name: {}", target.display()))?;
    // renameのatomicityを保つため、targetと同一filesystem上へ作成する
    let (temporary_path, mut temporary_file) = create_temporary_file(parent, file_name)?;

    // 途中失敗時のtemporary file cleanupを共通化するため結果を保持する
    let result = (|| -> Result<()> {
        temporary_file
            .set_permissions(fs::Permissions::from_mode(TEMP_FILE_MODE))
            .with_context(|| {
                format!(
                    "failed to set temporary file permissions: {}",
                    temporary_path.display()
                )
            })?;
        temporary_file.write_all(contents).with_context(|| {
            format!(
                "failed to write temporary file: {}",
                temporary_path.display()
            )
        })?;
        temporary_file.sync_all().with_context(|| {
            format!(
                "failed to sync temporary file: {}",
                temporary_path.display()
            )
        })?;
        drop(temporary_file);

        validate_replace_target(target)?;
        fs::rename(&temporary_path, target).with_context(|| {
            format!(
                "failed to replace {} with {}",
                target.display(),
                temporary_path.display()
            )
        })?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

/// symlink追跡を避け、missingまたは通常fileだけを置換対象として許可する
fn validate_replace_target(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => bail!(
            "atomic save target must be a regular file or missing: {}",
            target.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect atomic save target: {}", target.display())),
    }
}

/// create_newで衝突を検出しながら同一directoryへtemporary fileを確保する
fn create_temporary_file(parent: &Path, file_name: &OsStr) -> Result<(PathBuf, fs::File)> {
    for _ in 0..128 {
        let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(".tmp-{}-{sequence}", process::id()));
        let temporary_path = parent.join(temporary_name);

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(TEMP_FILE_MODE)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to create temporary file in {}", parent.display())
                });
            }
        }
    }

    bail!(
        "failed to allocate a unique temporary file in {}",
        parent.display()
    )
}

// --------------------------------------------------
// Tests
// --------------------------------------------------
