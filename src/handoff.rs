// ==================================================
// @file src/handoff.rs
// @brief Shell handoff protocol handling
// ==================================================

use std::io::{self, BufRead, Write};
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{is_show_cad_status_enabled, load_installed_paths};

/// stdin / stdoutを使用してZsh handoff選択を取得する
pub fn run_handoff() -> Result<bool> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    run_handoff_with(&mut stdin.lock(), &mut stdout.lock())
}

/// installed configからshow-cad-status設定を取得する
pub fn run_show_cad_status_enabled() -> Result<bool> {
    let paths = load_installed_paths()?;
    run_show_cad_status_enabled_from(&paths.eiyah_config)
}

/// handoff promptのIOを差し替えてYes / No protocolを処理する
fn run_handoff_with(input: &mut impl BufRead, output: &mut impl Write) -> Result<bool> {
    loop {
        write!(output, "Switch to Zsh? [Y/n] ")?;
        output.flush()?;

        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            bail!("handoff input closed before an answer was received");
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {}
        }
    }
}

/// config pathを差し替えてinternal status protocolを評価する
fn run_show_cad_status_enabled_from(config_path: &Path) -> Result<bool> {
    is_show_cad_status_enabled(config_path)
}

// --------------------------------------------------
// Tests
// --------------------------------------------------
