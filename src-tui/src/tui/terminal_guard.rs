use anyhow::Result;
use std::io::stdout;

pub struct TerminalGuard {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    suspended: bool,
}

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        enable()?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            suspended: false,
        })
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

    pub const fn terminal_mut(
        &mut self,
    ) -> &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>> {
        &mut self.terminal
    }

    /// Disable raw mode and leave the alternate screen so an external editor
    /// can take over the terminal. Idempotent.
    pub fn suspend(&mut self) -> Result<()> {
        if self.suspended {
            return Ok(());
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
