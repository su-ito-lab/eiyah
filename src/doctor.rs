// ==================================================
// @file src/doctor.rs
// @brief Eiyah installation diagnostics
// ==================================================

use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::config::{
    ResolvedPaths, load_config, load_installed_paths_for_ui_from_home, os_release_value,
    runtime_home,
};
use crate::ui::print_warning;

// issueがない場合のdoctor result
const HEALTHY_MESSAGE: &str = "Your system is ready to use Eiyah.";

// 全診断項目を収集しWarningまたはsuccess messageを表示する
pub(super) fn run_doctor() -> Result<bool> {
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
        diagnose_installation(home, &mut issues);
    }
    diagnose_login_shell(&mut issues);
    diagnose_host_compatibility(&mut issues);

    report_doctor(issues, print_warning)
}

// installation pathを復元し、失敗時は根本issueだけを記録する
fn diagnose_installation(home: &Path, issues: &mut Vec<String>) {
    match load_installed_paths_for_ui_from_home(home) {
        Ok(paths) => diagnose_installed_paths(home, &paths, issues),
        Err(error) => issues.push(error.to_string()),
    }
}

// collected issueをhealthy resultまたはWarning列へ変換する
fn report_doctor(issues: Vec<String>, mut warn: impl FnMut(&str)) -> Result<bool> {
    if issues.is_empty() {
        crate::ui::print_detail(HEALTHY_MESSAGE)?;
        Ok(true)
    } else {
        for issue in issues {
            warn(&issue);
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
fn diagnose_installed_paths(home: &Path, paths: &ResolvedPaths, issues: &mut Vec<String>) {
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
            "Eiyah config is missing or invalid: {}",
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
    if !is_executable(&status_binary) {
        issues.push(format!(
            "show-cad-status binary is not executable: {}",
            status_binary.display()
        ));
    }
    if !is_expected_symlink(&status_entry, &status_binary) {
        issues.push(format!(
            "show-cad-status command link is invalid: {}",
            status_entry.display()
        ));
    }
}

// Public Eiyah entryがinstalled binaryを直接指すことを診断する
fn diagnose_eiyah_symlink(home: &Path, binary: &Path, issues: &mut Vec<String>) {
    let public_entry = home.join(".local/bin/eiyah");
    if !is_expected_symlink(&public_entry, binary) {
        issues.push(format!(
            "Eiyah command link is invalid: {}",
            public_entry.display()
        ));
    }
}

// configured login shellがcsh / tcsh familyであることを診断する
fn diagnose_login_shell(issues: &mut Vec<String>) {
    let shell = env::var_os("SHELL").map(PathBuf::from);
    diagnose_login_shell_value(shell.as_deref(), issues);
}

// login shellの実値を保持してcsh / tcsh familyを診断する
fn diagnose_login_shell_value(shell: Option<&Path>, issues: &mut Vec<String>) {
    let valid = shell
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "csh" || name == "tcsh");
    if !valid {
        let actual = shell
            .as_deref()
            .map(|value| value.as_os_str().to_string_lossy().into_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "N/A".to_owned());
        issues.push(format!("Login shell is not csh or tcsh: {actual}"));
    }
}

// OS / architecture / glibcのhost compatibilityを診断する
fn diagnose_host_compatibility(issues: &mut Vec<String>) {
    let os = os_release_value("PRETTY_NAME");
    let os_id = os_release_value("ID");
    let os_version = os_release_value("VERSION_ID");
    diagnose_os_compatibility(
        os.as_deref(),
        os_id.as_deref(),
        os_version.as_deref(),
        issues,
    );
    let architecture = command_line("uname", &["-m"]);
    let glibc = command_line("getconf", &["GNU_LIBC_VERSION"]);
    let glibc_compatible = glibc
        .as_deref()
        .and_then(|value| value.split_whitespace().last())
        .and_then(parse_major_minor)
        .is_some_and(|version| version >= (2, 28));
    diagnose_host_compatibility_values(
        architecture.as_deref(),
        glibc.as_deref(),
        glibc_compatible,
        issues,
    );
}

// os-releaseの必要field取得後にAlmaLinux 8.x compatibilityを診断する
fn diagnose_os_compatibility(
    os: Option<&str>,
    os_id: Option<&str>,
    os_version: Option<&str>,
    issues: &mut Vec<String>,
) {
    let (Some(os), Some(os_id), Some(os_version)) = (os, os_id, os_version) else {
        issues.push("OS information could not be read: /etc/os-release".to_owned());
        return;
    };
    if os_id != "almalinux" || os_version.split('.').next() != Some("8") {
        issues.push(format!("Unsupported OS: {os}"));
    }
}

// host情報の実値を原因単位のWarningへ変換する
fn diagnose_host_compatibility_values(
    architecture: Option<&str>,
    glibc: Option<&str>,
    glibc_compatible: bool,
    issues: &mut Vec<String>,
) {
    if architecture.is_none() {
        issues.push("Architecture could not be read: uname -m".to_owned());
    } else if architecture.as_deref() != Some("x86_64") {
        issues.push(format!(
            "Unsupported architecture: {}",
            architecture.expect("architecture value was checked")
        ));
    }
    if glibc.is_none() {
        issues.push("Host glibc could not be read: getconf GNU_LIBC_VERSION".to_owned());
    } else if !glibc_compatible {
        issues.push(format!(
            "Host glibc is too old: {}",
            glibc.expect("glibc value was checked")
        ));
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
    let output = Command::new(executable).args(arguments).output().ok()?;
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

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

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

        assert_eq!(
            issues,
            [format!(
                "Eiyah command link is invalid: {}",
                public_entry.display()
            )]
        );
        fs::remove_dir_all(home)?;
        Ok(())
    }

    #[test]
    // login shellとhost incompatibilityを実値付きの個別Warningへ変換する
    fn reports_specific_runtime_values() {
        let mut issues = Vec::new();
        diagnose_login_shell_value(Some(Path::new("/bin/bash")), &mut issues);
        diagnose_os_compatibility(Some("Other Linux"), Some("other"), Some("1"), &mut issues);
        diagnose_host_compatibility_values(Some("aarch64"), Some("glibc 2.17"), false, &mut issues);
        assert_eq!(
            issues,
            [
                "Login shell is not csh or tcsh: /bin/bash",
                "Unsupported OS: Other Linux",
                "Unsupported architecture: aarch64",
                "Host glibc is too old: glibc 2.17",
            ]
        );
    }

    #[test]
    // runtime情報取得failureをunsupported判定と区別して取得元付きで報告する
    fn reports_runtime_information_failures() {
        let mut issues = Vec::new();
        diagnose_os_compatibility(None, None, None, &mut issues);
        diagnose_host_compatibility_values(None, None, false, &mut issues);
        assert_eq!(
            issues,
            [
                "OS information could not be read: /etc/os-release",
                "Architecture could not be read: uname -m",
                "Host glibc could not be read: getconf GNU_LIBC_VERSION",
            ]
        );
    }

    #[test]
    // PRETTY_NAMEがあってもIDまたはVERSION_ID欠落を取得failureとして報告する
    fn reports_missing_os_compatibility_fields() {
        for (os_id, os_version) in [(None, Some("8.10")), (Some("almalinux"), None)] {
            let mut issues = Vec::new();
            diagnose_os_compatibility(Some("AlmaLinux 8.10"), os_id, os_version, &mut issues);
            assert_eq!(
                issues,
                ["OS information could not be read: /etc/os-release"]
            );
        }
    }

    #[test]
    // doctor path recoveryでconfigと同じmissing command detailをWarning候補へ保持する
    fn reports_missing_eiyah_command_for_doctor_recovery() -> Result<()> {
        let home = std::env::temp_dir().join(format!(
            "eiyah-doctor-missing-command-{}",
            std::process::id()
        ));
        let mut issues = Vec::new();
        diagnose_installation(&home, &mut issues);
        assert_eq!(
            issues,
            [format!(
                "Eiyah installation information could not be read: Eiyah command was not found at {}",
                home.join(".local/bin/eiyah").display()
            )]
        );
        Ok(())
    }

    #[test]
    // healthy resultとissue時のWarningだけをexactに出力する
    fn reports_doctor_results() -> Result<()> {
        let (result, output) = crate::ui::capture_stdout(|| report_doctor(Vec::new(), |_| {}));
        assert!(result?);
        assert_eq!(output, "Your system is ready to use Eiyah.\n");

        let warnings = std::cell::RefCell::new(Vec::new());
        assert!(!report_doctor(
            vec!["first".to_owned(), "second".to_owned()],
            |warning| warnings.borrow_mut().push(warning.to_owned()),
        )?);
        assert_eq!(*warnings.borrow(), ["first", "second"]);
        Ok(())
    }
}
