// ==================================================
// @file src/main.rs
// @brief Eiyah command-line entry point
// ==================================================

use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

/// 設定modelと配置pathの解決・永続化を提供するmodule
pub mod config;
/// tcshからのhandoff protocolを提供するmodule
pub mod handoff;
/// install状態の判定を提供するmodule
pub mod install;
/// filesystem変更の記録とrollbackを提供するmodule
pub mod transaction;

use config::{
    collect_system_config, load_config, load_installed_paths, os_release_value, print_config,
    runtime_home, set_show_cad_status,
};
use handoff::{run_handoff, run_show_cad_status_enabled};
use install::{run_install, run_uninstall, run_update};

// 実装済みのPublic / Internal CLI
#[derive(Debug, Parser)]
#[command(name = "eiyah", version)]
struct Cli {
    // 実行するEiyah command
    #[command(subcommand)]
    command: Command,
}

// 現在本実装を持つcommand
#[derive(Debug, Subcommand)]
enum Command {
    // bootstrap binaryからinitial installを実行するinternal command
    #[command(name = "__install", hide = true)]
    Install,
    // managed environmentを削除するinternal command
    #[command(name = "__uninstall", hide = true)]
    Uninstall,
    // Public ReleaseからEiyah binaryを更新する
    Update,
    // Eiyahとsystem configurationを表示する
    Config,
    // Eiyah installationを診断する
    Doctor,
    // CAD status表示または表示設定を操作する
    ShowCadStatus {
        // show-cad-status設定の変更action
        #[command(subcommand)]
        action: Option<ShowCadStatusAction>,
    },
    // tcshからZshへ移行するか問い合わせるinternal command
    #[command(name = "__handoff", hide = true)]
    Handoff,
    // show-cad-status設定をexit statusで返すinternal command
    #[command(name = "__show-cad-status-enabled", hide = true)]
    ShowCadStatusEnabled,
}

// show-cad-status設定へ適用するaction
#[derive(Debug, Subcommand)]
enum ShowCadStatusAction {
    // shell handoff前のstatus表示を有効化する
    Enable,
    // shell handoff前のstatus表示を無効化する
    Disable,
}

// CLI errorを表示してcontractに対応するexit statusへ変換する
fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            print_error(&format!("{error:#}"));
            ExitCode::from(1)
        }
    }
}

// Current Branchで実装済みのcommandだけをdispatchする
fn run(cli: Cli) -> Result<u8> {
    run_with(
        cli,
        run_install,
        run_uninstall,
        run_update,
        run_handoff,
        run_show_cad_status_enabled,
        run_doctor,
        run_show_cad_status,
    )
}

// command dependencyを差し替え可能にしてCLI dispatchを実行する
fn run_with(
    cli: Cli,
    mut install: impl FnMut() -> Result<()>,
    mut uninstall: impl FnMut() -> Result<()>,
    mut update: impl FnMut() -> Result<()>,
    mut handoff: impl FnMut() -> Result<bool>,
    mut show_cad_status_enabled: impl FnMut() -> Result<bool>,
    mut doctor: impl FnMut() -> Result<bool>,
    mut show_cad_status: impl FnMut() -> Result<u8>,
) -> Result<u8> {
    match cli.command {
        Command::Install => {
            install()?;
            Ok(0)
        }
        Command::Uninstall => {
            uninstall()?;
            Ok(0)
        }
        Command::Update => {
            update()?;
            Ok(0)
        }
        Command::Config => {
            print_config(&collect_system_config()?)?;
            Ok(0)
        }
        Command::Doctor => Ok(if doctor()? { 0 } else { 1 }),
        Command::ShowCadStatus { action: None } => show_cad_status(),
        Command::ShowCadStatus {
            action: Some(ShowCadStatusAction::Enable),
        } => {
            set_show_cad_status(true)?;
            Ok(0)
        }
        Command::ShowCadStatus {
            action: Some(ShowCadStatusAction::Disable),
        } => {
            set_show_cad_status(false)?;
            Ok(0)
        }
        Command::Handoff => match handoff() {
            Ok(true) => Ok(0),
            Ok(false) => Ok(1),
            Err(error) => {
                print_error(&format!("{error:#}"));
                Ok(2)
            }
        },
        Command::ShowCadStatusEnabled => match show_cad_status_enabled() {
            Ok(true) => Ok(0),
            Ok(false) => Ok(1),
            Err(error) => {
                print_error(&format!("{error:#}"));
                Ok(2)
            }
        },
    }
}

// Public show-cad-status entryを継承streamで実行する
fn run_show_cad_status() -> Result<u8> {
    let executable = runtime_home()?.join(".local/bin/show-cad-status");
    let status = ProcessCommand::new(&executable)
        .status()
        .with_context(|| format!("failed to execute {}", executable.display()))?;
    child_exit_status(status)
}

// child process statusをPublic CLIのexit statusへ変換する
fn child_exit_status(status: std::process::ExitStatus) -> Result<u8> {
    match status.code() {
        Some(code) if (0..=u8::MAX as i32).contains(&code) => Ok(code as u8),
        Some(code) => bail!("show-cad-status returned unsupported exit status {code}"),
        None => bail!("show-cad-status terminated by signal"),
    }
}

// 全診断項目を収集しWarningまたはsuccess messageを表示する
fn run_doctor() -> Result<bool> {
    let mut issues = Vec::new();
    let home = match runtime_home() {
        Ok(home) => Some(home),
        Err(error) => {
            issues.push(format!("HOME is unavailable: {error:#}"));
            None
        }
    };

    if let Some(home) = &home {
        diagnose_dotfiles(home, &mut issues);
        match load_installed_paths() {
            Ok(paths) => diagnose_installed_paths(home, &paths, &mut issues),
            Err(error) => issues.push(format!(
                "installed Eiyah paths could not be recovered: {error:#}"
            )),
        }
    }
    diagnose_login_shell(&mut issues);
    diagnose_host_compatibility(&mut issues);

    if issues.is_empty() {
        println!("Your system is ready to use Eiyah.");
        Ok(true)
    } else {
        for issue in issues {
            print_warning(&issue);
        }
        Ok(false)
    }
}

// HOMEだけで判定可能なdotfiles directoryを診断する
fn diagnose_dotfiles(home: &Path, issues: &mut Vec<String>) {
    let dotfiles = home.join(".dotfiles");
    if !dotfiles.is_dir() {
        issues.push(format!(
            "dotfiles directory is missing: {}",
            dotfiles.display()
        ));
    }
}

// 復元済みpathへ依存するmanaged artifactを診断する
fn diagnose_installed_paths(home: &Path, paths: &config::ResolvedPaths, issues: &mut Vec<String>) {
    let binary = paths.eiyah_prefix.join("bin/eiyah");
    if !is_executable(&binary) {
        issues.push(format!(
            "Eiyah binary is not executable: {}",
            binary.display()
        ));
    }
    diagnose_eiyah_symlink(home, &binary, issues);
    if load_config(&paths.eiyah_config).is_err() {
        issues.push(format!(
            "config.toml is missing or invalid: {}",
            paths.eiyah_config.display()
        ));
    }
    for (name, path) in [
        ("Pixi", paths.pixi_home.join("bin/pixi")),
        ("Zsh", paths.pixi_home.join("bin/zsh")),
    ] {
        if !is_executable(&path) {
            issues.push(format!("{name} is not executable: {}", path.display()));
        }
    }

    let status_binary = paths.eiyah_prefix.join("bin/show-cad-status");
    let status_entry = home.join(".local/bin/show-cad-status");
    if !is_executable(&status_binary) || !is_expected_symlink(&status_entry, &status_binary) {
        issues.push("show-cad-status installation is invalid".to_owned());
    }
}

// Public Eiyah entryがinstalled binaryを直接指すことを診断する
fn diagnose_eiyah_symlink(home: &Path, binary: &Path, issues: &mut Vec<String>) {
    let public_entry = home.join(".local/bin/eiyah");
    if !is_expected_symlink(&public_entry, binary) {
        issues.push("Eiyah public symlink is invalid".to_owned());
    }
}

// configured login shellがcsh / tcsh familyであることを診断する
fn diagnose_login_shell(issues: &mut Vec<String>) {
    let shell = env::var_os("SHELL").map(PathBuf::from);
    let valid = shell
        .as_deref()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "csh" || name == "tcsh");
    if !valid {
        issues.push("login shell is not csh or tcsh".to_owned());
    }
}

// OS / architecture / glibcのhost compatibilityを診断する
fn diagnose_host_compatibility(issues: &mut Vec<String>) {
    let os_compatible = os_release_value("ID").as_deref() == Some("almalinux")
        && os_release_value("VERSION_ID")
            .as_deref()
            .and_then(|version| version.split('.').next())
            == Some("8");
    let architecture = command_line("uname", &["-m"]);
    let glibc = command_line("getconf", &["GNU_LIBC_VERSION"]);
    let glibc_compatible = glibc
        .as_deref()
        .and_then(|value| value.split_whitespace().last())
        .and_then(parse_major_minor)
        .is_some_and(|version| version >= (2, 28));
    if !os_compatible || architecture.as_deref() != Some("x86_64") || !glibc_compatible {
        issues
            .push("host is not compatible with AlmaLinux 8.x / x86_64 / glibc >= 2.28".to_owned());
    }
}

// executable pathがregular fileかつexecute bit付きか確認する
fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// symlinkがexpected absolute targetを直接指すことを確認する
fn is_expected_symlink(path: &Path, expected: &Path) -> bool {
    fs::read_link(path)
        .map(|target| target.is_absolute() && target == expected)
        .unwrap_or(false)
}

// command成功時のstdout先頭行を取得する
fn command_line(executable: &str, arguments: &[&str]) -> Option<String> {
    let output = ProcessCommand::new(executable)
        .args(arguments)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
}

// major.minor versionを数値tupleへ変換する
fn parse_major_minor(value: &str) -> Option<(u64, u64)> {
    let mut fields = value.split('.');
    Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
}

// stderrのTTY / NO_COLOR契約に従ってWarningを表示する
pub(crate) fn print_warning(message: &str) {
    if io::stderr().is_terminal() && env::var_os("NO_COLOR").is_none() {
        eprintln!("\x1b[33mWarning:\x1b[0m {message}");
    } else {
        eprintln!("Warning: {message}");
    }
}

// stderrのTTY / NO_COLOR契約に従ってErrorを表示する
fn print_error(message: &str) {
    if io::stderr().is_terminal() && env::var_os("NO_COLOR").is_none() {
        eprintln!("\x1b[31mError:\x1b[0m {message}");
    } else {
        eprintln!("Error: {message}");
    }
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::os::unix::process::ExitStatusExt;

    use clap::CommandFactory;

    use super::*;

    #[test]
    // 実装済みのPublic / Internal commandをparseできることを検証する
    fn parses_commands() {
        for arguments in [
            vec!["eiyah", "update"],
            vec!["eiyah", "__install"],
            vec!["eiyah", "__uninstall"],
            vec!["eiyah", "config"],
            vec!["eiyah", "doctor"],
            vec!["eiyah", "show-cad-status"],
            vec!["eiyah", "show-cad-status", "enable"],
            vec!["eiyah", "show-cad-status", "disable"],
            vec!["eiyah", "__handoff"],
            vec!["eiyah", "__show-cad-status-enabled"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_ok());
        }
    }

    #[test]
    // internal commandをhelp表示から隠すことを検証する
    fn hides_internal_commands_from_help() {
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("__install"));
        assert!(!help.contains("__uninstall"));
        assert!(!help.contains("__handoff"));
        assert!(!help.contains("__show-cad-status-enabled"));
    }

    #[test]
    // glibc compatibility判定用のmajor.minor parseを検証する
    fn parses_major_minor_version() {
        assert_eq!(parse_major_minor("2.28"), Some((2, 28)));
        assert_eq!(parse_major_minor("invalid"), None);
    }

    #[test]
    // Eiyah public symlinkのtarget不一致をdoctor issueへ集約することを検証する
    fn diagnoses_mismatched_eiyah_symlink() -> Result<()> {
        let home =
            std::env::temp_dir().join(format!("eiyah-doctor-symlink-test-{}", std::process::id()));
        let public_entry = home.join(".local/bin/eiyah");
        let expected = home.join("data/eiyah/bin/eiyah");
        fs::create_dir_all(public_entry.parent().unwrap())?;
        symlink(home.join("other/bin/eiyah"), &public_entry)?;

        let mut issues = Vec::new();
        diagnose_eiyah_symlink(&home, &expected, &mut issues);

        assert_eq!(issues, ["Eiyah public symlink is invalid"]);
        fs::remove_dir_all(home)?;
        Ok(())
    }

    #[test]
    // internal protocolとdoctorのCLI exit status mappingを検証する
    fn maps_runtime_dispatch_exit_statuses() -> Result<()> {
        for (command, result, expected) in [
            ("__handoff", Ok(true), 0),
            ("__handoff", Ok(false), 1),
            ("__handoff", Err(anyhow::anyhow!("handoff error")), 2),
            ("__show-cad-status-enabled", Ok(true), 0),
            ("__show-cad-status-enabled", Ok(false), 1),
            (
                "__show-cad-status-enabled",
                Err(anyhow::anyhow!("config error")),
                2,
            ),
        ] {
            let cli = Cli::try_parse_from(["eiyah", command])?;
            let mut result = Some(result);
            let status = if command == "__handoff" {
                run_with(
                    cli,
                    || Ok(()),
                    || Ok(()),
                    || Ok(()),
                    || result.take().unwrap(),
                    || Ok(false),
                    || Ok(false),
                    || Ok(0),
                )?
            } else {
                run_with(
                    cli,
                    || Ok(()),
                    || Ok(()),
                    || Ok(()),
                    || Ok(false),
                    || result.take().unwrap(),
                    || Ok(false),
                    || Ok(0),
                )?
            };
            assert_eq!(status, expected);
        }

        for (healthy, expected) in [(true, 0), (false, 1)] {
            let cli = Cli::try_parse_from(["eiyah", "doctor"])?;
            assert_eq!(
                run_with(
                    cli,
                    || Ok(()),
                    || Ok(()),
                    || Ok(()),
                    || Ok(false),
                    || Ok(false),
                    || Ok(healthy),
                    || Ok(0),
                )?,
                expected
            );
        }
        Ok(())
    }

    #[test]
    // actionなしshow-cad-statusがchild exit statusをそのまま返すことを検証する
    fn propagates_show_cad_status_child_exit_status() -> Result<()> {
        let child_status = std::process::ExitStatus::from_raw(37 << 8);
        assert_eq!(child_exit_status(child_status)?, 37);

        let cli = Cli::try_parse_from(["eiyah", "show-cad-status"])?;
        assert_eq!(
            run_with(
                cli,
                || Ok(()),
                || Ok(()),
                || Ok(()),
                || Ok(false),
                || Ok(false),
                || Ok(false),
                || Ok(37),
            )?,
            37
        );
        Ok(())
    }

    #[test]
    // update commandの成功とerrorをPublic CLI statusへ反映することを検証する
    fn dispatches_update() -> Result<()> {
        let cli = Cli::try_parse_from(["eiyah", "update"])?;
        assert_eq!(
            run_with(
                cli,
                || Ok(()),
                || Ok(()),
                || Ok(()),
                || Ok(false),
                || Ok(false),
                || Ok(false),
                || Ok(0),
            )?,
            0
        );

        let cli = Cli::try_parse_from(["eiyah", "update"])?;
        assert!(
            run_with(
                cli,
                || Ok(()),
                || Ok(()),
                || Err(anyhow::anyhow!("update failed")),
                || Ok(false),
                || Ok(false),
                || Ok(false),
                || Ok(0),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    // __installをinternal implementationへdispatchする
    fn dispatches_install() -> Result<()> {
        let cli = Cli::try_parse_from(["eiyah", "__install"])?;
        let mut called = false;
        let status = run_with(
            cli,
            || {
                called = true;
                Ok(())
            },
            || Ok(()),
            || Ok(()),
            || Ok(false),
            || Ok(false),
            || Ok(false),
            || Ok(0),
        )?;
        assert_eq!(status, 0);
        assert!(called);
        Ok(())
    }

    #[test]
    // __uninstallをinternal implementationへdispatchする
    fn dispatches_uninstall() -> Result<()> {
        let cli = Cli::try_parse_from(["eiyah", "__uninstall"])?;
        let mut called = false;
        let status = run_with(
            cli,
            || Ok(()),
            || {
                called = true;
                Ok(())
            },
            || Ok(()),
            || Ok(false),
            || Ok(false),
            || Ok(false),
            || Ok(0),
        )?;
        assert_eq!(status, 0);
        assert!(called);
        Ok(())
    }
}
