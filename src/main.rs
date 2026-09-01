// ==================================================
// @file src/main.rs
// @brief Eiyah command-line entry point
// ==================================================

use std::env;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

/// 設定modelと配置pathの解決・永続化を提供するmodule
pub mod config;
/// Eiyah installationの診断を提供するmodule
mod doctor;
/// tcshからのhandoff protocolを提供するmodule
pub mod handoff;
/// install / update / uninstall lifecycleを提供するmodule
mod lifecycle;
/// filesystem変更の記録とrollbackを提供するmodule
pub mod transaction;

use config::{collect_system_config, print_config, runtime_home, set_show_cad_status};
use doctor::run_doctor;
use handoff::{run_handoff, run_show_cad_status_enabled};
use lifecycle::{run_install, run_uninstall, run_update};

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
    Uninstall {
        // shell bootstrapへ返すfinal cleanup plan path
        #[arg(long, value_name = "PATH")]
        cleanup_plan: PathBuf,
    },
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
    mut uninstall: impl FnMut(&Path) -> Result<()>,
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
        Command::Uninstall { cleanup_plan } => {
            uninstall(&cleanup_plan)?;
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
    use std::os::unix::process::ExitStatusExt;

    use clap::CommandFactory;

    use super::*;

    #[test]
    // 実装済みのPublic / Internal commandをparseできることを検証する
    fn parses_commands() {
        for arguments in [
            vec!["eiyah", "update"],
            vec!["eiyah", "__install"],
            vec!["eiyah", "__uninstall", "--cleanup-plan", "/tmp/plan"],
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
                    |_| Ok(()),
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
                    |_| Ok(()),
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
                    |_| Ok(()),
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
                |_| Ok(()),
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
                |_| Ok(()),
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
                |_| Ok(()),
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
            |_| Ok(()),
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
        let cli = Cli::try_parse_from([
            "eiyah",
            "__uninstall",
            "--cleanup-plan",
            "/tmp/uninstall-plan",
        ])?;
        let mut called = false;
        let mut received = None;
        let status = run_with(
            cli,
            || Ok(()),
            |cleanup_plan| {
                called = true;
                received = Some(cleanup_plan.to_path_buf());
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
        assert_eq!(received, Some(PathBuf::from("/tmp/uninstall-plan")));
        Ok(())
    }
}
