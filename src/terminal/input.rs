//! Bounded, cancellable console input; secret bytes are never echoed.
use std::io::{self, IsTerminal as _, Write as _};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::watch;
use unicode_segmentation::UnicodeSegmentation as _;
use zeroize::Zeroizing;

use super::session::Session;

pub fn available() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

pub struct Input {
    _session: Session,
    shutdown: watch::Receiver<bool>,
}

impl Input {
    pub fn open(shutdown: watch::Receiver<bool>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            available(),
            "interactive setup requires terminal stdin and stderr; use --password-stdin for redirected input"
        );
        Ok(Self {
            _session: Session::enter(false)?,
            shutdown,
        })
    }

    pub fn line(&self, prompt: &str, secret: bool) -> anyhow::Result<Zeroizing<String>> {
        let mut out = io::stderr();
        echo_line(&mut out, prompt, "")?;
        out.flush()?;
        let mut value = Zeroizing::new(String::new());
        loop {
            anyhow::ensure!(!*self.shutdown.borrow(), "setup cancelled");
            if !event::poll(Duration::from_millis(50))? {
                continue;
            }
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Esc => anyhow::bail!("setup cancelled"),
                    KeyCode::Char('c' | 'd' | 'z')
                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        anyhow::bail!("setup cancelled")
                    }
                    KeyCode::Enter => {
                        write!(out, "\r\n")?;
                        return Ok(value);
                    }
                    KeyCode::Backspace => {
                        if let Some((index, _)) = value.grapheme_indices(true).next_back() {
                            value.truncate(index);
                        }
                    }
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && !ch.is_control() =>
                    {
                        anyhow::ensure!(
                            value.len() + ch.len_utf8() <= 4096,
                            "input exceeds 4096 bytes"
                        );
                        value.push(ch);
                    }
                    _ => {}
                },
                // Reject multiline paste: it must never submit a confirmation or command.
                Event::Paste(mut text) => {
                    use zeroize::Zeroize as _;
                    let valid =
                        !text.chars().any(char::is_control) && value.len() + text.len() <= 4096;
                    if valid {
                        value.push_str(&text);
                    }
                    text.zeroize();
                    anyhow::ensure!(valid, "paste must be one line and at most 4096 bytes");
                }
                _ => {}
            }
            if !secret {
                echo_line(&mut out, prompt, &value)?;
            }
            out.flush()?;
        }
    }
}

/// Clip by grapheme/display width, avoiding wrapped-field editing and split emoji.
fn echo_line(out: &mut impl io::Write, prompt: &str, value: &str) -> io::Result<()> {
    let columns = usize::from(
        crossterm::terminal::size()
            .map_or(80, |size| size.0)
            .saturating_sub(1),
    )
    .max(1);
    let text = format!("{prompt}{}", super::display_text(value));
    let mut used = 0;
    let mut tail = Vec::new();
    for grapheme in text.graphemes(true).rev() {
        let width = ratatui::text::Line::from(grapheme).width();
        if used + width > columns {
            break;
        }
        tail.push(grapheme);
        used += width;
    }
    write!(out, "\r{:columns$}\r", "")?;
    for grapheme in tail.into_iter().rev() {
        write!(out, "{grapheme}")?;
    }
    Ok(())
}

/// Read a single bounded line from a pipe, with no password in argv/history.
/// Deliberately accepts EOF after a nonempty value for secret-file inputs.
pub fn password_stdin() -> anyhow::Result<Zeroizing<String>> {
    use std::io::{BufRead as _, Read as _};
    anyhow::ensure!(
        !io::stdin().is_terminal(),
        "--password-stdin requires redirected stdin"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    io::stdin()
        .lock()
        .take(4098)
        .read_until(b'\n', &mut bytes)?;
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= 4098,
        "password input is empty or exceeds 4096 bytes"
    );
    while bytes.last().is_some_and(|ch| matches!(ch, b'\r' | b'\n')) {
        bytes.pop();
    }
    anyhow::ensure!(
        bytes.len() <= 4096 && !bytes.is_empty(),
        "password input is empty or exceeds 4096 bytes"
    );
    Ok(Zeroizing::new(std::str::from_utf8(&bytes)?.to_owned()))
}
