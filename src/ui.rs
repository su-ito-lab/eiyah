// ==================================================
// @file src/ui.rs
// @brief Shared user-facing output formatting
// ==================================================

use std::env;
use std::io::{self, IsTerminal, Write};

#[cfg(test)]
use std::cell::RefCell;

#[cfg(test)]
thread_local! {
    // lifecycle testがproductionのstdout経路を直接検証するためのcapture先
    static TEST_STDOUT: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

// ANSI style reset
const ANSI_RESET: &str = "\x1b[0m";
// operation prefix用bright blue
const ANSI_OPERATION: &str = "\x1b[94m";
// operation message用bold
const ANSI_BOLD: &str = "\x1b[1m";
// Error label用bold bright red
const ANSI_ERROR: &str = "\x1b[1;91m";
// Warning label用bold bright yellow
const ANSI_WARNING: &str = "\x1b[1;93m";
// Hint label用bold bright cyan
const ANSI_HINT: &str = "\x1b[1;96m";

// 対象streamがTTYかつNO_COLOR未設定の場合だけstyleを有効にする
fn style_enabled(is_terminal: bool, no_color: bool) -> bool {
    is_terminal && !no_color
}

// stdout自身のTTY状態からoperation styleを決定する
pub(crate) fn stdout_style_enabled() -> bool {
    style_enabled(
        io::stdout().is_terminal(),
        env::var_os("NO_COLOR").is_some(),
    )
}

// Homebrew基調のoperation headingを指定outputへ書き出す
pub(crate) fn write_operation(
    output: &mut impl Write,
    message: &str,
    styled: bool,
) -> io::Result<()> {
    if styled {
        writeln!(
            output,
            "{ANSI_OPERATION}==>{ANSI_RESET} {ANSI_BOLD}{message}{ANSI_RESET}"
        )
    } else {
        writeln!(output, "==> {message}")
    }
}

// stdoutへoperation間の空行とheadingを出力する
pub(crate) fn print_operation(message: &str) -> io::Result<()> {
    #[cfg(test)]
    if TEST_STDOUT.with(|output| output.borrow().is_some()) {
        return TEST_STDOUT.with(|output| {
            let mut output = output.borrow_mut();
            let output = output.as_mut().expect("test stdout is active");
            writeln!(output)?;
            write_operation(output, message, false)
        });
    }
    let mut output = io::stdout().lock();
    writeln!(output)?;
    write_operation(&mut output, message, stdout_style_enabled())
}

// stdoutへ先頭の空行を伴わないoperation headingを出力する
pub(crate) fn print_first_operation(message: &str) -> io::Result<()> {
    #[cfg(test)]
    if TEST_STDOUT.with(|output| output.borrow().is_some()) {
        return TEST_STDOUT.with(|output| {
            write_operation(
                output.borrow_mut().as_mut().expect("test stdout is active"),
                message,
                false,
            )
        });
    }
    write_operation(&mut io::stdout().lock(), message, stdout_style_enabled())
}

// stdoutへoperation detailまたは通常結果をdefault styleで出力する
pub(crate) fn print_detail(message: &str) -> io::Result<()> {
    #[cfg(test)]
    if TEST_STDOUT.with(|output| output.borrow().is_some()) {
        return TEST_STDOUT.with(|output| {
            writeln!(
                output.borrow_mut().as_mut().expect("test stdout is active"),
                "{message}"
            )
        });
    }
    writeln!(io::stdout().lock(), "{message}")
}

// lifecycleのproduction出力経路をtest thread内でcaptureする
#[cfg(test)]
pub(crate) fn capture_stdout<T>(run: impl FnOnce() -> T) -> (T, String) {
    TEST_STDOUT.with(|output| {
        assert!(output.borrow().is_none(), "test stdout capture is nested");
        *output.borrow_mut() = Some(Vec::new());
    });
    let result = run();
    let output = TEST_STDOUT.with(|output| output.borrow_mut().take().unwrap());
    (result, String::from_utf8(output).unwrap())
}

// diagnostic labelだけへ指定styleを適用した1行を構成する
fn write_diagnostic(
    output: &mut impl Write,
    label: &str,
    message: &str,
    styled: bool,
) -> io::Result<()> {
    if styled {
        let label_style = match label {
            "Error" => ANSI_ERROR,
            "Warning" => ANSI_WARNING,
            "Hint" => ANSI_HINT,
            _ => "",
        };
        writeln!(output, "{label_style}{label}{ANSI_RESET}: {message}")
    } else {
        writeln!(output, "{label}: {message}")
    }
}

// stderr自身のTTY状態に従ってWarningを表示する
pub(crate) fn print_warning(message: &str) {
    write_diagnostic(
        &mut io::stderr().lock(),
        "Warning",
        message,
        style_enabled(
            io::stderr().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
        ),
    )
    .expect("failed printing to stderr");
}

// stderr自身のTTY状態に従ってErrorを表示する
pub(crate) fn print_error(message: &str) {
    write_diagnostic(
        &mut io::stderr().lock(),
        "Error",
        message,
        style_enabled(
            io::stderr().is_terminal(),
            env::var_os("NO_COLOR").is_some(),
        ),
    )
    .expect("failed printing to stderr");
}

// --------------------------------------------------
// Tests
// --------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // operation prefixとmessageへ独立したstyleを適用する
    fn formats_operation_output() {
        let mut plain = Vec::new();
        write_operation(&mut plain, "Install Eiyah", false).unwrap();
        assert_eq!(plain, b"==> Install Eiyah\n");

        let mut styled = Vec::new();
        write_operation(&mut styled, "Install Eiyah", true).unwrap();
        assert_eq!(styled, b"\x1b[94m==>\x1b[0m \x1b[1mInstall Eiyah\x1b[0m\n");
    }

    #[test]
    // diagnosticはlabelだけをstyleしてcolonとmessageをdefaultに戻す
    fn formats_diagnostic_output() {
        for (label, style) in [
            ("Error", ANSI_ERROR),
            ("Warning", ANSI_WARNING),
            ("Hint", ANSI_HINT),
        ] {
            let mut plain = Vec::new();
            write_diagnostic(&mut plain, label, "message", false).unwrap();
            assert_eq!(plain, format!("{label}: message\n").as_bytes());

            let mut styled = Vec::new();
            write_diagnostic(&mut styled, label, "message", true).unwrap();
            assert_eq!(
                styled,
                format!("{style}{label}{ANSI_RESET}: message\n").as_bytes()
            );
        }
    }

    #[test]
    // streamのTTY状態とNO_COLORを独立条件として扱う
    fn enables_style_only_for_tty_without_no_color() {
        assert!(style_enabled(true, false));
        assert!(!style_enabled(false, false));
        assert!(!style_enabled(true, true));
    }
}
