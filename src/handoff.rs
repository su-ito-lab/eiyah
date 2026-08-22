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

// handoff promptのIOを差し替えてYes / No protocolを処理する
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

// config pathを差し替えてinternal status protocolを評価する
fn run_show_cad_status_enabled_from(config_path: &Path) -> Result<bool> {
    is_show_cad_status_enabled(config_path)
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{Cursor, Error, ErrorKind};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::config::{Config, save_config};

    use super::*;

    // 並列test間でconfig pathが衝突しないための連番
    static TEST_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    // write errorをhandoffへ返すtest writer
    struct FailingWriter;

    impl Write for FailingWriter {
        // prompt writeを常に失敗させる
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(Error::new(ErrorKind::BrokenPipe, "test write failure"))
        }

        // test writerはbufferを持たないため成功扱いにする
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // test専用config pathを作成する
    fn config_path() -> PathBuf {
        let sequence = TEST_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "eiyah-handoff-config-{}-{sequence}.toml",
            std::process::id()
        ))
    }

    #[test]
    // EnterとASCIIの全accepted inputを検証する
    fn accepts_all_ascii_yes_and_no_inputs() -> Result<()> {
        for (input, expected) in [
            ("\n", true),
            ("y\n", true),
            ("Y\n", true),
            ("yes\n", true),
            ("Yes\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("N\n", false),
            ("no\n", false),
            ("No\n", false),
            ("NO\n", false),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                run_handoff_with(&mut Cursor::new(input), &mut output)?,
                expected
            );
        }
        Ok(())
    }

    #[test]
    // 日本語・全角入力をinvalidとして扱いUTF-8 errorなしで再試行することを検証する
    fn retries_japanese_and_full_width_input() -> Result<()> {
        let mut output = Vec::new();
        assert!(run_handoff_with(
            &mut Cursor::new("はい\nｙ\nyes\n"),
            &mut output
        )?);
        assert_eq!(
            String::from_utf8(output)?
                .matches("Switch to Zsh? [Y/n]")
                .count(),
            3
        );
        Ok(())
    }

    #[test]
    // invalid input後にpromptを再試行することを検証する
    fn retries_invalid_input() -> Result<()> {
        let mut output = Vec::new();
        assert!(!run_handoff_with(
            &mut Cursor::new("invalid\nno\n"),
            &mut output
        )?);
        assert_eq!(
            String::from_utf8(output)?
                .matches("Switch to Zsh? [Y/n]")
                .count(),
            2
        );
        Ok(())
    }

    #[test]
    // promptのIO errorをprotocol errorとして返すことを検証する
    fn reports_io_error() {
        assert!(run_handoff_with(&mut Cursor::new("yes\n"), &mut FailingWriter).is_err());
    }

    #[test]
    // configのenabled / disabledとparse errorを検証する
    fn reports_show_cad_status_config_state() -> Result<()> {
        let path = config_path();
        save_config(
            &path,
            &Config {
                show_cad_status: true,
            },
        )?;
        assert!(run_show_cad_status_enabled_from(&path)?);
        save_config(
            &path,
            &Config {
                show_cad_status: false,
            },
        )?;
        assert!(!run_show_cad_status_enabled_from(&path)?);
        fs::write(&path, b"invalid")?;
        assert!(run_show_cad_status_enabled_from(&path).is_err());
        fs::remove_file(path)?;
        Ok(())
    }
}
