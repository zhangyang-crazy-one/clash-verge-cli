use anyhow::Result;
use std::io::stdout;

pub struct TerminalGuard {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
}

impl TerminalGuard {
    pub fn new() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            crossterm::cursor::Hide
        )?;
        let backend = ratatui::backend::CrosstermBackend::new(stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal })
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
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
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
