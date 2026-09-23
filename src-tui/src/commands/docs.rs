//! `completions` and `man`: generated from the clap definition, so they
//! always match the installed binary.

use std::path::Path;

use anyhow::Context as _;
use clap::CommandFactory as _;

use crate::cli::Cli;

/// Print the completion script for `shell` to stdout.
pub fn completions(shell: clap_complete::Shell) -> anyhow::Result<()> {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    let mut script = Vec::new();
    clap_complete::generate(shell, &mut command, name, &mut script);
    write_stdout(&script)
}

/// Print the top-level man page, or write one page per command to `dir`.
pub fn man(dir: Option<&Path>) -> anyhow::Result<()> {
    let command = Cli::command();
    match dir {
        None => {
            let mut page = Vec::new();
            clap_mangen::Man::new(command)
                .render(&mut page)
                .context("failed to render the man page")?;
            write_stdout(&page)
        }
        Some(dir) => {
            std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
            clap_mangen::generate_to(command, dir)
                .with_context(|| format!("failed to write man pages to {}", dir.display()))
        }
    }
}

/// Write to stdout; a reader that closed early (`| head`) is not an error.
fn write_stdout(bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error.into()),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn man_pages_cover_every_subcommand() {
        let dir = std::env::temp_dir().join(format!("cv-man-{}", uuid::Uuid::new_v4()));
        man(Some(&dir)).unwrap();
        for page in [
            "clash-verge-cli.1",
            "clash-verge-cli-proxy.1",
            "clash-verge-cli-profile-use.1",
        ] {
            assert!(dir.join(page).exists(), "missing {page}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
