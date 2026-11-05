//! One owner for terminal modes. Signal handlers are registered before entry.
use std::io::{self, Write as _};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};

pub struct Session {
    screen: bool,
    paste: bool,
    #[cfg(windows)]
    input_mode: (crossterm_winapi::ConsoleMode, u32),
    #[cfg(windows)]
    output_mode: Option<(crossterm_winapi::ConsoleMode, u32)>,
}

impl Session {
    pub fn enter(screen: bool) -> io::Result<Self> {
        #[cfg(windows)]
        let input_mode = {
            use crossterm_winapi::{ConsoleMode, Handle};
            let mode = ConsoleMode::from(Handle::current_in_handle()?);
            let original = mode.mode()?;
            (mode, original)
        };
        #[cfg(windows)]
        let output_mode = {
            use crossterm_winapi::{ConsoleMode, Handle};
            Handle::current_out_handle().ok().and_then(|handle| {
                let mode = ConsoleMode::from(handle);
                mode.mode().ok().map(|original| (mode, original))
            })
        };
        crossterm::terminal::enable_raw_mode()?;
        #[cfg(windows)]
        let paste = output_mode.as_ref().is_some_and(|(mode, original)| {
            // ENABLE_VIRTUAL_TERMINAL_PROCESSING from the Windows console API.
            // Re-enable for each session: Crossterm caches this capability.
            mode.set_mode(*original | 0x0004).is_ok()
        });
        #[cfg(not(windows))]
        let paste = !matches!(std::env::var("TERM").as_deref(), Ok("dumb"));
        let session = Self {
            screen,
            paste,
            #[cfg(windows)]
            input_mode,
            #[cfg(windows)]
            output_mode,
        };
        // Construct the guard before fallible output so partial entry cleans up.
        if screen {
            execute!(io::stdout(), EnterAlternateScreen, Hide)?;
            if paste {
                execute!(io::stdout(), EnableBracketedPaste)?;
            }
        } else if paste {
            execute!(io::stderr(), EnableBracketedPaste)?;
        }
        Ok(session)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.screen {
            if self.paste {
                let _ = execute!(io::stdout(), DisableBracketedPaste);
            }
            let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        } else if self.paste {
            let _ = execute!(io::stderr(), DisableBracketedPaste);
        }
        #[cfg(windows)]
        {
            let _ = self.input_mode.0.set_mode(self.input_mode.1);
            if let Some((mode, original)) = &self.output_mode {
                let _ = mode.set_mode(*original);
            }
        }
        #[cfg(not(windows))]
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
    }
}
