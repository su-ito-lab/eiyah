// ==================================================
// @file src/config.rs
// @brief Configuration and installation path handling
// ==================================================

use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::transaction::LockGuard;

// user設定を保存するTOML file名
const CONFIG_FILE_NAME: &str = "config.toml";
// 各XDG base directory配下で使用するapplication directory名
const EIYAH_DIRECTORY_NAME: &str = "eiyah";
// install後の配置情報を保存するTOML file名
const INSTALL_METADATA_FILE_NAME: &str = "install.toml";
// atomic saveのtemporary fileへ設定するpermission
const TEMP_FILE_MODE: u32 = 0o600;

// process内でtemporary file名の重複を避けるための連番
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

// diagnosticsへ表示するPublic repository URL
const ORIGIN: &str = "https://github.com/su-ito-lab/eiyah";

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

    // 検証済みのXDG base pathへEiyah固有のrelative pathを付加する
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
    // metadataの全pathが空でないabsolute pathであることを保証する
    fn validate(&self) -> Result<()> {
        validate_absolute_path("config-home", &self.config_home)?;
        validate_absolute_path("data-home", &self.data_home)?;
        validate_absolute_path("state-home", &self.state_home)?;
        validate_absolute_path("cache-home", &self.cache_home)?;
        Ok(())
    }
}

impl From<&ResolvedPaths> for InstallMetadata {
    // derived pathを除外し、正本となる4つのXDG base pathだけを抽出する
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

// environment取得元を差し替え可能にしてXDG fallback規則を一元化する
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

// fallbackできない必須environment pathへ共通の絶対path制約を適用する
fn required_absolute_environment_path(name: &str, value: Option<OsString>) -> Result<PathBuf> {
    let value = value.with_context(|| format!("{name} is not set"))?;
    let path = PathBuf::from(value);
    validate_absolute_path(name, &path)?;
    Ok(path)
}

// XDG値が未設定または空の場合だけHOME配下のfallbackを選択する
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

// filesystem操作がcurrent directoryへ依存しないようpath制約を検証する
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

/// user設定fileを読み、厳格なschemaで`Config`へ変換する
pub fn load_config(path: &Path) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config: {}", path.display()))?;
    toml::from_str(&contents).with_context(|| format!("failed to parse config: {}", path.display()))
}

/// `Config`をTOML化し、既存fileを安全にatomic replacementする
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

/// 初回install用metadataを既存targetを置換せずに作成する
pub fn create_install_metadata(paths: &ResolvedPaths) -> Result<()> {
    create_install_metadata_with(paths, |_| Ok(()))
}

// commit直前のraceをtest可能にしつつ初回install用metadataを保存する
fn create_install_metadata_with<F>(paths: &ResolvedPaths, before_commit: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let metadata = InstallMetadata::from(paths);
    metadata.validate()?;
    let contents =
        toml::to_string(&metadata).context("failed to serialize installation metadata")?;
    let target = paths.eiyah_prefix.join(INSTALL_METADATA_FILE_NAME);
    create_initial_file_with(&target, contents.as_bytes(), before_commit)
}

/// 初回install用configを`show-cad-status = true`で新規作成する
pub fn create_initial_config(paths: &ResolvedPaths) -> Result<()> {
    create_initial_config_with(paths, |_| Ok(()))
}

// commit直前のraceをtest可能にしてinitial configを作成する
fn create_initial_config_with<F>(paths: &ResolvedPaths, before_commit: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let contents = toml::to_string(&Config {
        show_cad_status: true,
    })
    .context("failed to serialize initial config")?;
    create_initial_file_with(&paths.eiyah_config, contents.as_bytes(), before_commit)
}

// same-directory temporary fileからinitial targetをatomic no-replaceで作成する
fn create_initial_file_with<F>(target: &Path, contents: &[u8], before_commit: F) -> Result<()>
where
    F: FnOnce(&Path) -> Result<()>,
{
    validate_initial_target(&target)?;

    let parent = target
        .parent()
        .with_context(|| format!("target has no parent directory: {}", target.display()))?;
    validate_existing_directory(parent)?;
    let file_name = target
        .file_name()
        .with_context(|| format!("target has no file name: {}", target.display()))?;
    let (temporary_path, mut temporary_file) = create_temporary_file(parent, file_name)?;
    // cleanupとcommitを今回create_newしたinodeへ限定するためのidentity
    let created_temporary = temporary_file.metadata().with_context(|| {
        format!(
            "failed to inspect temporary file: {}",
            temporary_path.display()
        )
    })?;

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
        validate_initial_target(&target)?;
        before_commit(&temporary_path)?;
        validate_same_inode(&temporary_path, &created_temporary)?;
        rename_without_replace(&temporary_path, &target).with_context(|| {
            format!(
                "failed to create installation metadata: {}",
                target.display()
            )
        })
    })();

    if result.is_err() {
        let _ = remove_file_if_same_inode(&temporary_path, &created_temporary);
    }
    result
}

// pathが作成時と同じinodeを指す場合だけtemporary fileをcleanupする
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

// temporary pathが今回作成したinodeのままであることを保証する
fn validate_same_inode(path: &Path, created: &fs::Metadata) -> Result<()> {
    let current = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect temporary file: {}", path.display()))?;
    if !same_inode(&current, created) {
        bail!("temporary file was replaced: {}", path.display());
    }
    Ok(())
}

// filesystem deviceとinodeで同一fileを判定する
fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

// initial install targetが種類を問わず存在しないことを保証する
fn validate_initial_target(target: &Path) -> Result<()> {
    match fs::symlink_metadata(target) {
        Ok(_) => bail!(
            "initial install target already exists: {}",
            target.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect initial target: {}", target.display())),
    }
}

// metadata parentが既存のnon-symlink directoryであることを保証する
fn validate_existing_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect directory: {}", path.display()))?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("path must be a non-symlink directory: {}", path.display());
    }
    Ok(())
}

// Linuxのrenameat2でfinal targetのatomic no-replaceを保証する
fn rename_without_replace(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("source path contains a NUL byte"))?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| anyhow::anyhow!("destination path contains a NUL byte"))?;

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

// 同一directoryのtemporary fileを介して、通常fileだけをatomic置換する
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
    // replacement検知とcleanupを今回create_newしたinodeへ限定するidentity
    let created_temporary = temporary_file.metadata().with_context(|| {
        format!(
            "failed to inspect temporary file: {}",
            temporary_path.display()
        )
    })?;

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
        validate_replace_target(target)?;
        validate_same_inode(&temporary_path, &created_temporary)?;
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
        let _ = remove_file_if_same_inode(&temporary_path, &created_temporary);
    }
    result
}

// symlink追跡を避け、missingまたは通常fileだけを置換対象として許可する
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

// create_newで衝突を検出しながら同一directoryへtemporary fileを確保する
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
// Runtime Configuration
// --------------------------------------------------

/// runtime diagnosticsへ表示するsystem情報
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemConfig {
    /// 実行中のPublic Eiyah version
    pub eiyah_version: String,
    /// Public Eiyah repository URL
    pub origin: String,
    /// install metadataから復元したEiyah prefix
    pub eiyah_prefix: PathBuf,
    /// install metadataから復元したconfig path
    pub eiyah_config: PathBuf,
    /// user HOME配下のPrivate dotfiles path
    pub dotfiles: PathBuf,
    /// install metadataから復元したPixi home
    pub pixi_home: PathBuf,
    /// Pixi version output
    pub pixi: Option<String>,
    /// Zsh version output
    pub zsh: Option<String>,
    /// process environmentのlogin shell
    pub login_shell: Option<String>,
    /// Bash version outputの先頭行
    pub bash: Option<String>,
    /// Curl version outputの先頭行
    pub curl: Option<String>,
    /// os-releaseのPRETTY_NAME
    pub os: Option<String>,
    /// host kernel release
    pub kernel: Option<String>,
    /// host architecture
    pub architecture: Option<String>,
    /// host glibc version
    pub host_glibc: Option<String>,
}

/// installed configをexclusive lock内で更新する
pub fn set_show_cad_status(enabled: bool) -> Result<()> {
    let home = runtime_home()?;
    set_show_cad_status_from_home(&home, enabled)
}

// HOMEを差し替え可能にしてinstalled configを更新する
fn set_show_cad_status_from_home(home: &Path, enabled: bool) -> Result<()> {
    let paths = load_installed_paths_from_home(home)?;
    let config_path = paths.eiyah_config.clone();
    let _lock = LockGuard::acquire(&paths.state_home).map_err(|error| {
        let detail = if error.to_string() == "another Eiyah operation is already running." {
            error.to_string()
        } else {
            "exclusive access could not be acquired".to_owned()
        };
        config_update_error(&config_path, &detail)
    })?;
    let mut config = load_config(&config_path)
        .map_err(|_| config_update_error(&config_path, "the config could not be read or parsed"))?;
    config.show_cad_status = enabled;
    save_config(&config_path, &config)
        .map_err(|_| config_update_error(&config_path, "the config changes could not be saved"))
}

// config更新failureを保存方式に依存しないuser-facing errorへ変換する
fn config_update_error(path: &Path, detail: &str) -> anyhow::Error {
    anyhow::Error::new(crate::ui::UserFacingError::new(
        format!(
            "failed to update Eiyah config: {}: {detail}",
            path.display()
        ),
        Vec::new(),
        Vec::new(),
    ))
}

/// 指定configのshow-cad-status設定を返す
pub fn is_show_cad_status_enabled(config_path: &Path) -> Result<bool> {
    Ok(load_config(config_path)?.show_cad_status)
}

/// installed pathと取得可能なruntime情報を収集する
pub fn collect_system_config() -> Result<SystemConfig> {
    let home = runtime_home()?;
    let paths = load_installed_paths_from_home(&home)?;

    Ok(SystemConfig {
        eiyah_version: env!("CARGO_PKG_VERSION").to_owned(),
        origin: ORIGIN.to_owned(),
        eiyah_prefix: paths.eiyah_prefix.clone(),
        eiyah_config: paths.eiyah_config.clone(),
        dotfiles: home.join(".dotfiles"),
        pixi_home: paths.pixi_home.clone(),
        pixi: command_first_line(&paths.pixi_home.join("bin/pixi"), &["--version"]),
        zsh: command_first_line(&paths.pixi_home.join("bin/zsh"), &["--version"]),
        login_shell: optional_environment_string(env::var_os("SHELL")),
        bash: command_first_line(Path::new("/usr/bin/bash"), &["--version"]),
        curl: command_first_line(Path::new("curl"), &["--version"]),
        os: os_release_value("PRETTY_NAME"),
        kernel: command_first_line(Path::new("uname"), &["-r"]),
        architecture: command_first_line(Path::new("uname"), &["-m"]),
        host_glibc: command_first_line(Path::new("getconf"), &["GNU_LIBC_VERSION"]),
    })
}

/// [SystemConfig]をcontractで定めた順序とsectionへ出力する
pub fn print_config(config: &SystemConfig) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    print_config_to(&mut output, config)
}

/// runtime command間でinstalled metadataからpathを復元する
pub(crate) fn load_installed_paths() -> Result<ResolvedPaths> {
    let home = runtime_home()?;
    load_installed_paths_from_home(&home)
}

/// `HOME`を差し替え可能にしてinstalled path discoveryを行う
pub(crate) fn load_installed_paths_from_home(home: &Path) -> Result<ResolvedPaths> {
    let public_entry = home.join(".local/bin/eiyah");
    let metadata_path = discover_install_metadata(&public_entry)?;
    ResolvedPaths::from_install_metadata(load_install_metadata(&metadata_path)?)
}

/// runtime operationに必要なabsolute `HOME`を取得する
pub(crate) fn runtime_home() -> Result<PathBuf> {
    required_absolute_environment_path("HOME", env::var_os("HOME"))
}

// config表示をtest可能なwriterへ出力する
fn print_config_to(mut output: impl Write, config: &SystemConfig) -> Result<()> {
    writeln!(output, "EIYAH_VERSION: {}", config.eiyah_version)?;
    writeln!(output, "ORIGIN: {}", config.origin)?;
    writeln!(output, "EIYAH_PREFIX: {}", config.eiyah_prefix.display())?;
    writeln!(output, "EIYAH_CONFIG: {}", config.eiyah_config.display())?;
    writeln!(output, "DOTFILES: {}", config.dotfiles.display())?;
    writeln!(output, "PIXI_HOME: {}", config.pixi_home.display())?;
    writeln!(output)?;
    writeln!(output, "Pixi: {}", option_display(&config.pixi))?;
    writeln!(output, "Zsh: {}", option_display(&config.zsh))?;
    writeln!(
        output,
        "Login shell: {}",
        option_display(&config.login_shell)
    )?;
    writeln!(output, "Bash: {}", option_display(&config.bash))?;
    writeln!(output, "Curl: {}", option_display(&config.curl))?;
    writeln!(output)?;
    writeln!(output, "OS: {}", option_display(&config.os))?;
    writeln!(output, "Kernel: {}", option_display(&config.kernel))?;
    writeln!(
        output,
        "Architecture: {}",
        option_display(&config.architecture)
    )?;
    writeln!(output, "Host glibc: {}", option_display(&config.host_glibc))?;
    Ok(())
}

// optional runtime valueをN/A fallback付きで表示する
fn option_display(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("N/A")
}

// emptyまたはnon-UTF environment値を取得不能として扱う
fn optional_environment_string(value: Option<OsString>) -> Option<String> {
    value
        .filter(|value| !value.is_empty())
        .and_then(|value| value.into_string().ok())
}

// external command成功時のstdout先頭行だけを取得する
fn command_first_line(executable: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new(executable).args(arguments).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

/// `os-release`から指定fieldのquoteを除いた値を取得する
pub(crate) fn os_release_value(name: &str) -> Option<String> {
    let contents = fs::read_to_string("/etc/os-release").ok()?;
    parse_os_release_value(&contents, name)
}

// os-release textから指定fieldを抽出する
fn parse_os_release_value(contents: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    let value = contents
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))?;
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    (!value.is_empty()).then(|| value.to_owned())
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::Result;

    use super::*;

    // 並列test間でtemporary directory名が衝突しないための連番
    static TEST_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

    // 各test専用directoryの作成とcleanupを所有するfixture
    struct TestDirectory {
        // fixtureが所有するtemporary directory path
        path: PathBuf,
    }

    impl TestDirectory {
        // process IDと連番から衝突しないtest directoryを作成する
        fn new() -> Result<Self> {
            let sequence = TEST_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("eiyah-config-test-{}-{sequence}", process::id()));
            fs::create_dir(&path)?;
            Ok(Self { path })
        }
    }

    impl Drop for TestDirectory {
        // test成否にかかわらずfixture配下だけを可能な限りcleanupする
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    // process environmentを変更せずに任意のenvironment値でpath解決を検証する
    fn resolve_with(values: &[(&str, &str)]) -> Result<ResolvedPaths> {
        let values: HashMap<&str, OsString> = values
            .iter()
            .map(|(name, value)| (*name, OsString::from(value)))
            .collect();
        resolve_paths_from(|name| values.get(name).cloned())
    }

    // atomic save test用に全XDG base pathをfixture配下へ閉じ込める
    fn paths_under(root: &Path) -> ResolvedPaths {
        ResolvedPaths::from_homes(
            root.join("config"),
            root.join("data"),
            root.join("state"),
            root.join("cache"),
        )
    }

    #[test]
    // XDG未設定時にHOME配下の規定pathが選ばれることを検証する
    fn resolve_paths_uses_xdg_fallbacks() -> Result<()> {
        let paths = resolve_with(&[("HOME", "/home/tester")])?;
        assert_eq!(paths.config_home, Path::new("/home/tester/.config"));
        assert_eq!(paths.data_home, Path::new("/home/tester/.local/share"));
        assert_eq!(paths.state_home, Path::new("/home/tester/.local/state"));
        assert_eq!(paths.cache_home, Path::new("/home/tester/.cache"));
        assert_eq!(
            paths.eiyah_prefix,
            Path::new("/home/tester/.local/share/eiyah")
        );
        assert_eq!(
            paths.eiyah_config,
            Path::new("/home/tester/.config/eiyah/config.toml")
        );
        assert_eq!(
            paths.pixi_home,
            Path::new("/home/tester/.local/share/eiyah/pixi")
        );
        Ok(())
    }

    #[test]
    // 空のXDG値を未設定と同様に扱うことを検証する
    fn resolve_paths_treats_empty_xdg_values_as_unset() -> Result<()> {
        let paths = resolve_with(&[
            ("HOME", "/home/tester"),
            ("XDG_CONFIG_HOME", ""),
            ("XDG_DATA_HOME", ""),
            ("XDG_STATE_HOME", ""),
            ("XDG_CACHE_HOME", ""),
        ])?;
        assert_eq!(paths.config_home, Path::new("/home/tester/.config"));
        assert_eq!(paths.data_home, Path::new("/home/tester/.local/share"));
        assert_eq!(paths.state_home, Path::new("/home/tester/.local/state"));
        assert_eq!(paths.cache_home, Path::new("/home/tester/.cache"));
        Ok(())
    }

    #[test]
    // environment pathを不必要にUTF-8へ変換しないことを検証する
    fn resolve_paths_preserves_non_utf8_environment_paths() -> Result<()> {
        let data_home = OsString::from_vec(b"/xdg/data-\xff".to_vec());
        let paths = resolve_paths_from(|name| match name {
            "HOME" => Some(OsString::from("/home/tester")),
            "XDG_DATA_HOME" => Some(data_home.clone()),
            _ => None,
        })?;
        assert_eq!(paths.data_home.as_os_str(), data_home);
        assert_eq!(paths.eiyah_prefix, PathBuf::from(data_home).join("eiyah"));
        Ok(())
    }

    #[test]
    // absolute XDG値がfallbackより優先されることを検証する
    fn resolve_paths_uses_absolute_xdg_values() -> Result<()> {
        let paths = resolve_with(&[
            ("HOME", "/home/tester"),
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("XDG_DATA_HOME", "/xdg/data"),
            ("XDG_STATE_HOME", "/xdg/state"),
            ("XDG_CACHE_HOME", "/xdg/cache"),
        ])?;
        assert_eq!(paths.config_home, Path::new("/xdg/config"));
        assert_eq!(paths.data_home, Path::new("/xdg/data"));
        assert_eq!(paths.state_home, Path::new("/xdg/state"));
        assert_eq!(paths.cache_home, Path::new("/xdg/cache"));
        Ok(())
    }

    #[test]
    // HOMEとXDGへ適用する必須・absolute path制約を検証する
    fn resolve_paths_rejects_invalid_home_and_xdg_values() {
        assert!(resolve_with(&[]).is_err());
        assert!(resolve_with(&[("HOME", "")]).is_err());
        assert!(resolve_with(&[("HOME", "relative")]).is_err());
        assert!(resolve_with(&[("HOME", "/home/tester"), ("XDG_DATA_HOME", "relative")]).is_err());
    }

    #[test]
    // metadataのbase pathからderived pathが正しく復元されることを検証する
    fn install_metadata_converts_to_resolved_paths() -> Result<()> {
        let metadata = InstallMetadata {
            config_home: PathBuf::from("/xdg/config"),
            data_home: PathBuf::from("/xdg/data"),
            state_home: PathBuf::from("/xdg/state"),
            cache_home: PathBuf::from("/xdg/cache"),
        };
        let paths = ResolvedPaths::from_install_metadata(metadata)?;
        assert_eq!(paths.eiyah_prefix, Path::new("/xdg/data/eiyah"));
        assert_eq!(
            paths.eiyah_config,
            Path::new("/xdg/config/eiyah/config.toml")
        );
        assert_eq!(paths.pixi_home, Path::new("/xdg/data/eiyah/pixi"));
        Ok(())
    }

    #[test]
    // targetの存在に依存せず正常・broken symlinkを探索できることを検証する
    fn discover_install_metadata_accepts_normal_and_broken_symlinks() -> Result<()> {
        let directory = TestDirectory::new()?;
        let prefix = directory.path.join("prefix");
        let target = prefix.join("bin/eiyah");
        let normal_entry = directory.path.join("normal-eiyah");
        fs::create_dir_all(target.parent().unwrap())?;
        fs::write(&target, b"binary")?;
        symlink(&target, &normal_entry)?;
        assert_eq!(
            discover_install_metadata(&normal_entry)?,
            prefix.join("install.toml")
        );

        let broken_target = directory.path.join("broken-prefix/bin/eiyah");
        let broken_entry = directory.path.join("broken-eiyah");
        symlink(&broken_target, &broken_entry)?;
        assert_eq!(
            discover_install_metadata(&broken_entry)?,
            directory.path.join("broken-prefix/install.toml")
        );
        Ok(())
    }

    #[test]
    // metadata discoveryが不正なpublic entry pointを拒否することを検証する
    fn discover_install_metadata_rejects_invalid_entry_points() -> Result<()> {
        let directory = TestDirectory::new()?;
        let missing = directory.path.join("missing");
        assert!(discover_install_metadata(&missing).is_err());

        let regular = directory.path.join("regular");
        fs::write(&regular, b"not a symlink")?;
        assert!(discover_install_metadata(&regular).is_err());

        let relative = directory.path.join("relative");
        symlink("prefix/bin/eiyah", &relative)?;
        assert!(discover_install_metadata(&relative).is_err());

        let wrong_shape = directory.path.join("wrong-shape");
        symlink(directory.path.join("prefix/eiyah"), &wrong_shape)?;
        assert!(discover_install_metadata(&wrong_shape).is_err());
        Ok(())
    }

    #[test]
    // Configのbool値がdefaultなしで往復できることを検証する
    fn config_round_trips_true_and_false() -> Result<()> {
        for value in [true, false] {
            let config = Config {
                show_cad_status: value,
            };
            let serialized = toml::to_string(&config)?;
            assert_eq!(toml::from_str::<Config>(&serialized)?, config);
        }
        Ok(())
    }

    #[test]
    // Configがschema外または不完全なTOMLを拒否することを検証する
    fn config_rejects_missing_unknown_duplicate_and_malformed_fields() {
        assert!(toml::from_str::<Config>("").is_err());
        assert!(toml::from_str::<Config>("show-cad-status = true\nunknown = 1\n").is_err());
        assert!(
            toml::from_str::<Config>("show-cad-status = true\nshow-cad-status = false\n").is_err()
        );
        assert!(toml::from_str::<Config>("show-cad-status = [\n").is_err());
    }

    #[test]
    // InstallMetadataの厳格なschemaとabsolute path制約を検証する
    fn install_metadata_rejects_invalid_toml_and_paths() {
        let valid = concat!(
            "config-home = \"/config\"\n",
            "data-home = \"/data\"\n",
            "state-home = \"/state\"\n",
            "cache-home = \"/cache\"\n",
        );
        assert!(toml::from_str::<InstallMetadata>(valid).is_ok());
        assert!(toml::from_str::<InstallMetadata>(&format!("{valid}unknown = 1\n")).is_err());
        assert!(toml::from_str::<InstallMetadata>("config-home = \"/config\"\n").is_err());
        assert!(
            toml::from_str::<InstallMetadata>(&format!("{valid}config-home = \"/other-config\"\n"))
                .is_err()
        );

        let relative = InstallMetadata {
            config_home: PathBuf::from("relative"),
            data_home: PathBuf::from("/data"),
            state_home: PathBuf::from("/state"),
            cache_home: PathBuf::from("/cache"),
        };
        assert!(ResolvedPaths::from_install_metadata(relative).is_err());

        let empty = InstallMetadata {
            config_home: PathBuf::new(),
            data_home: PathBuf::from("/data"),
            state_home: PathBuf::from("/state"),
            cache_home: PathBuf::from("/cache"),
        };
        assert!(ResolvedPaths::from_install_metadata(empty).is_err());
    }

    #[test]
    // load_install_metadataがempty / relative pathを拒否することを検証する
    fn load_install_metadata_rejects_empty_and_relative_paths() -> Result<()> {
        let directory = TestDirectory::new()?;
        // empty pathを含むmetadataのload結果を直接確認するためのfixture
        let empty_path = directory.path.join("empty.toml");
        fs::write(
            &empty_path,
            concat!(
                "config-home = \"\"\n",
                "data-home = \"/data\"\n",
                "state-home = \"/state\"\n",
                "cache-home = \"/cache\"\n",
            ),
        )?;
        assert!(load_install_metadata(&empty_path).is_err());

        // relative pathを含むmetadataのload結果を直接確認するためのfixture
        let relative_path = directory.path.join("relative.toml");
        fs::write(
            &relative_path,
            concat!(
                "config-home = \"/config\"\n",
                "data-home = \"relative\"\n",
                "state-home = \"/state\"\n",
                "cache-home = \"/cache\"\n",
            ),
        )?;
        assert!(load_install_metadata(&relative_path).is_err());

        Ok(())
    }

    #[test]
    // load APIがmissing fileをerrorとして返すことを検証する
    fn load_functions_report_missing_files() {
        let missing = Path::new("/definitely/missing/eiyah-config.toml");
        assert!(load_config(missing).is_err());
        assert!(load_install_metadata(missing).is_err());
    }

    #[test]
    // atomic saveの作成・置換と0600 permissionを検証する
    fn atomic_save_creates_and_replaces_regular_files_with_mode_0600() -> Result<()> {
        let directory = TestDirectory::new()?;
        let config_path = directory.path.join("config.toml");
        save_config(
            &config_path,
            &Config {
                show_cad_status: true,
            },
        )?;
        assert_eq!(
            fs::metadata(&config_path)?.permissions().mode() & 0o777,
            TEMP_FILE_MODE
        );
        assert_eq!(load_config(&config_path)?.show_cad_status, true);

        save_config(
            &config_path,
            &Config {
                show_cad_status: false,
            },
        )?;
        assert_eq!(load_config(&config_path)?.show_cad_status, false);
        Ok(())
    }

    #[test]
    // atomic saveが危険なtargetとparent未作成を拒否することを検証する
    fn atomic_save_rejects_symlinks_directories_and_missing_parents() -> Result<()> {
        let directory = TestDirectory::new()?;
        let regular = directory.path.join("regular");
        fs::write(&regular, b"content")?;
        let symlink_path = directory.path.join("config.toml");
        symlink(&regular, &symlink_path)?;
        assert!(
            save_config(
                &symlink_path,
                &Config {
                    show_cad_status: true,
                }
            )
            .is_err()
        );

        let directory_target = directory.path.join("directory-target");
        fs::create_dir(&directory_target)?;
        assert!(
            save_config(
                &directory_target,
                &Config {
                    show_cad_status: true,
                }
            )
            .is_err()
        );

        assert!(
            save_config(
                &directory.path.join("missing/config.toml"),
                &Config {
                    show_cad_status: true,
                }
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    // install metadataが4つのbase pathだけを保存することを検証する
    fn install_metadata_saves_only_the_four_home_paths() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = paths_under(&directory.path);
        fs::create_dir_all(&paths.eiyah_prefix)?;
        save_install_metadata(&paths)?;

        let metadata_path = paths.eiyah_prefix.join("install.toml");
        let contents = fs::read_to_string(&metadata_path)?;
        assert_eq!(contents.lines().count(), 4);
        assert!(!contents.contains("version"));
        assert_eq!(
            load_install_metadata(&metadata_path)?,
            InstallMetadata::from(&paths)
        );
        Ok(())
    }

    #[test]
    // SystemConfigを所定のlabel・順序・空行で表示することを検証する
    fn system_config_uses_canonical_output_format() -> Result<()> {
        let config = SystemConfig {
            eiyah_version: "1.2.3".to_owned(),
            origin: "https://example.invalid/eiyah".to_owned(),
            eiyah_prefix: PathBuf::from("/data/eiyah"),
            eiyah_config: PathBuf::from("/config/eiyah/config.toml"),
            dotfiles: PathBuf::from("/home/tester/.dotfiles"),
            pixi_home: PathBuf::from("/data/eiyah/pixi"),
            pixi: Some("pixi 1".to_owned()),
            zsh: None,
            login_shell: Some("/bin/tcsh".to_owned()),
            bash: Some("GNU bash".to_owned()),
            curl: Some("curl 1".to_owned()),
            os: Some("AlmaLinux 8".to_owned()),
            kernel: Some("kernel".to_owned()),
            architecture: Some("x86_64".to_owned()),
            host_glibc: Some("glibc 2.28".to_owned()),
        };
        let mut output = Vec::new();
        print_config_to(&mut output, &config)?;
        assert_eq!(
            String::from_utf8(output)?,
            concat!(
                "EIYAH_VERSION: 1.2.3\n",
                "ORIGIN: https://example.invalid/eiyah\n",
                "EIYAH_PREFIX: /data/eiyah\n",
                "EIYAH_CONFIG: /config/eiyah/config.toml\n",
                "DOTFILES: /home/tester/.dotfiles\n",
                "PIXI_HOME: /data/eiyah/pixi\n",
                "\n",
                "Pixi: pixi 1\n",
                "Zsh: N/A\n",
                "Login shell: /bin/tcsh\n",
                "Bash: GNU bash\n",
                "Curl: curl 1\n",
                "\n",
                "OS: AlmaLinux 8\n",
                "Kernel: kernel\n",
                "Architecture: x86_64\n",
                "Host glibc: glibc 2.28\n",
            )
        );
        Ok(())
    }

    #[test]
    // os-releaseからquote付きPRETTY_NAMEを取得することを検証する
    fn parses_os_release_pretty_name() {
        let contents = "ID=almalinux\nPRETTY_NAME=\"AlmaLinux 8.10\"\n";
        assert_eq!(
            parse_os_release_value(contents, "PRETTY_NAME").as_deref(),
            Some("AlmaLinux 8.10")
        );
        assert_eq!(parse_os_release_value(contents, "MISSING"), None);
    }

    #[test]
    // empty SHELLを取得不能runtimeとしてN/A表示することを検証する
    fn treats_empty_login_shell_as_unavailable() {
        let login_shell = optional_environment_string(Some(OsString::new()));
        assert_eq!(login_shell, None);
        assert_eq!(option_display(&login_shell), "N/A");
    }

    #[test]
    // metadata由来pathとLockGuardを使用してshow-cad-status設定を更新することを検証する
    fn updates_show_cad_status_through_installed_metadata_and_lock() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let paths = paths_under(&directory.path.join("metadata-xdg"));
        let binary = paths.eiyah_prefix.join("bin/eiyah");
        let public_entry = home.join(".local/bin/eiyah");
        fs::create_dir_all(binary.parent().unwrap())?;
        fs::create_dir_all(public_entry.parent().unwrap())?;
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;
        symlink(&binary, &public_entry)?;
        save_install_metadata(&paths)?;
        save_config(
            &paths.eiyah_config,
            &Config {
                show_cad_status: false,
            },
        )?;

        let fallback_config = home.join(".config/eiyah/config.toml");
        fs::create_dir_all(fallback_config.parent().unwrap())?;
        save_config(
            &fallback_config,
            &Config {
                show_cad_status: false,
            },
        )?;

        set_show_cad_status_from_home(&home, true)?;
        assert!(load_config(&paths.eiyah_config)?.show_cad_status);
        assert!(!load_config(&fallback_config)?.show_cad_status);

        let _lock = LockGuard::acquire(&paths.state_home)?;
        let error = set_show_cad_status_from_home(&home, false).unwrap_err();
        assert!(error.to_string().starts_with(&format!(
            "failed to update Eiyah config: {}: ",
            paths.eiyah_config.display()
        )));
        assert!(!error.to_string().contains("temporary"));
        assert!(!error.to_string().contains("atomic"));
        assert!(load_config(&paths.eiyah_config)?.show_cad_status);
        Ok(())
    }

    #[test]
    // config save failureでatomic save等の内部実装用語を露出しない
    fn hides_config_save_implementation_details() -> Result<()> {
        let directory = TestDirectory::new()?;
        let home = directory.path.join("home");
        let paths = paths_under(&directory.path.join("metadata-xdg"));
        let binary = paths.eiyah_prefix.join("bin/eiyah");
        let public_entry = home.join(".local/bin/eiyah");
        let actual_config = directory.path.join("actual-config.toml");
        fs::create_dir_all(binary.parent().unwrap())?;
        fs::create_dir_all(public_entry.parent().unwrap())?;
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;
        symlink(&binary, &public_entry)?;
        save_install_metadata(&paths)?;
        save_config(
            &actual_config,
            &Config {
                show_cad_status: false,
            },
        )?;
        symlink(&actual_config, &paths.eiyah_config)?;

        let error = set_show_cad_status_from_home(&home, true).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!(
                "failed to update Eiyah config: {}: the config changes could not be saved",
                paths.eiyah_config.display()
            )
        );
        assert!(!error.to_string().contains("temporary"));
        assert!(!error.to_string().contains("atomic"));
        assert!(!load_config(&actual_config)?.show_cad_status);
        Ok(())
    }

    #[test]
    // 初回install metadataを4 pathだけでmode 0600の新規fileとして作成する
    fn creates_initial_install_metadata_without_replacement() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = paths_under(&directory.path);
        fs::create_dir_all(&paths.eiyah_prefix)?;

        create_install_metadata(&paths)?;

        let target = paths.eiyah_prefix.join(INSTALL_METADATA_FILE_NAME);
        assert_eq!(
            load_install_metadata(&target)?,
            InstallMetadata::from(&paths)
        );
        assert_eq!(fs::metadata(&target)?.permissions().mode() & 0o777, 0o600);
        assert!(create_install_metadata(&paths).is_err());
        Ok(())
    }

    #[test]
    // commit直前に出現したmetadataを上書きせず所有temporary fileだけをcleanupする
    fn preserves_install_metadata_created_during_commit_race() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = paths_under(&directory.path);
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let target = paths.eiyah_prefix.join(INSTALL_METADATA_FILE_NAME);

        assert!(
            create_install_metadata_with(&paths, |_| {
                fs::write(&target, b"concurrent metadata")?;
                Ok(())
            })
            .is_err()
        );

        assert_eq!(fs::read(&target)?, b"concurrent metadata");
        assert!(fs::read_dir(&paths.eiyah_prefix)?.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".install.toml.tmp-")
        }));
        Ok(())
    }

    #[test]
    // commit前に置換された他者所有temporary fileをcommitもcleanupもしない
    fn preserves_replaced_install_metadata_temporary_file() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = paths_under(&directory.path);
        fs::create_dir_all(&paths.eiyah_prefix)?;
        let mut replacement = None;

        assert!(
            create_install_metadata_with(&paths, |temporary| {
                fs::remove_file(temporary)?;
                fs::write(temporary, b"concurrent temporary")?;
                replacement = Some(temporary.to_path_buf());
                Ok(())
            })
            .is_err()
        );

        let replacement = replacement.unwrap();
        assert_eq!(fs::read(&replacement)?, b"concurrent temporary");
        assert!(!paths.eiyah_prefix.join(INSTALL_METADATA_FILE_NAME).exists());
        Ok(())
    }

    #[test]
    // initial configを明示的なtrueとmode 0600でatomic no-replace作成する
    fn creates_initial_config_without_replacement() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = paths_under(&directory.path);
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;

        create_initial_config(&paths)?;

        assert_eq!(
            load_config(&paths.eiyah_config)?,
            Config {
                show_cad_status: true
            }
        );
        assert_eq!(
            fs::metadata(&paths.eiyah_config)?.permissions().mode() & 0o777,
            0o600
        );
        assert!(create_initial_config(&paths).is_err());
        Ok(())
    }

    #[test]
    // config commit raceでexisting targetを上書きせずtemporaryだけをcleanupする
    fn preserves_initial_config_created_during_race() -> Result<()> {
        let directory = TestDirectory::new()?;
        let paths = paths_under(&directory.path);
        fs::create_dir_all(paths.eiyah_config.parent().unwrap())?;

        assert!(
            create_initial_config_with(&paths, |_| {
                fs::write(&paths.eiyah_config, b"concurrent config")?;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(fs::read(&paths.eiyah_config)?, b"concurrent config");
        Ok(())
    }
}
