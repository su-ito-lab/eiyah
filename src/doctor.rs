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
    ResolvedPaths, load_config, load_installed_paths, os_release_value, runtime_home,
};
use crate::print_warning;

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

        assert_eq!(issues, ["Eiyah public symlink is invalid"]);
        fs::remove_dir_all(home)?;
        Ok(())
    }
}
