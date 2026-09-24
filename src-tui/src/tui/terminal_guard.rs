use anyhow::Result;
use std::io::stdout;

pub struct TerminalGuard {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    suspended: bool,
    /// Mouse reporting is on (`mouse: true` in tui.yaml); it is turned off
    /// while suspended so an editor gets a normal terminal.
    mouse: bool,
}

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        enable()?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            suspended: false,
            mouse: false,
        })
    }

    /// A guard that never touched the terminal (no raw mode, no alternate
    /// screen, nothing restored on drop), for handler tests without a TTY.
    #[cfg(test)]
    pub fn detached() -> Self {
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, 80, 24)),
        };
        Self {
            terminal: ratatui::Terminal::with_options(backend, options).expect("fixed viewport needs no TTY"),
            suspended: true,
            mouse: false,
        }
    }

    /// Clear stale cells and reset Ratatui's diff buffers before a full repaint.
    pub fn reset_screen(&mut self) -> Result<()> {
        crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            crossterm::cursor::Hide
        )?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        self.terminal = ratatui::Terminal::new(backend)?;
        Ok(())
    }

    /// Self-heal when the terminal emulator resizes without delivering a
    /// `Event::Resize` (observed in Orca's embedded terminal): the diff
    /// renderer never touches rows outside the frame, so stale glyphs from
    /// earlier frames linger below it. Compare the backend's live size with
    /// the frame's cached area and force a full repaint on mismatch.
    pub fn sync_size_if_changed(&mut self) -> Result<()> {
        let live = ratatui::layout::Rect::from(self.terminal.size()?);
        let frame_area = self.terminal.get_frame().area();
        if live != frame_area {
            self.reset_screen()?;
        }
        Ok(())
    }

    pub const fn terminal_mut(
        &mut self,
    ) -> &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>> {
        &mut self.terminal
    }

    /// Report mouse events (wheel, clicks) as input.
    pub fn enable_mouse(&mut self) -> Result<()> {
        if !self.suspended {
            crossterm::execute!(self.terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
        }
        self.mouse = true;
        Ok(())
    }

    /// Disable raw mode and leave the alternate screen so an external editor
    /// can take over the terminal. Idempotent.
    pub fn suspend(&mut self) -> Result<()> {
        if self.suspended {
            return Ok(());
        }
        if self.mouse {
            crossterm::execute!(self.terminal.backend_mut(), crossterm::event::DisableMouseCapture)?;
        }
        crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        )?;
        crossterm::terminal::disable_raw_mode()?;
        self.suspended = true;
        Ok(())
    }

    /// Re-enter raw mode and alternate screen after an editor exits.
    /// Idempotent.
    pub fn resume(&mut self) -> Result<()> {
        if !self.suspended {
            return Ok(());
        }
        enable()?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        self.terminal = ratatui::Terminal::new(backend)?;
        self.suspended = false;
        if self.mouse {
            crossterm::execute!(self.terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
        }
        Ok(())
    }
}

fn enable() -> Result<()> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(
        stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0),
        crossterm::cursor::Hide
    )?;
    Ok(())
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.suspended {
            return;
        }
        if self.mouse {
            let _ = crossterm::execute!(self.terminal.backend_mut(), crossterm::event::DisableMouseCapture);
        }
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::IsTerminal;

    #[test]
    fn test_drop_restores() {
        // Skip when no real TTY is available (CI, redirected output, etc.)
        if !std::io::stdout().is_terminal() {
            eprintln!("skipping test_drop_restores: no TTY available");
            return;
        }
        {
            let _guard = TerminalGuard::new().unwrap();
        }
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }
}
