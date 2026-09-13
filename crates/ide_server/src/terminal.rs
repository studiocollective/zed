use crate::protocol::{Event, Outgoing};
use anyhow::{Context as _, Result};
use async_channel::Sender;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::path::Path;
use std::thread;

/// A shell running in a PTY whose raw output is forwarded to one connection.
pub struct TerminalSession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
}

impl TerminalSession {
    pub fn spawn(
        terminal_id: u64,
        cwd: &Path,
        cols: u16,
        rows: u16,
        outgoing: Sender<Outgoing>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening pty")?;

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut command = CommandBuilder::new(shell);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(command)
            .context("spawning shell")?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("cloning pty reader")?;
        let writer = pair.master.take_writer().context("taking pty writer")?;

        thread::Builder::new()
            .name(format!("ide-terminal-{terminal_id}"))
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            let event = Outgoing::Event(Event::TerminalOutput {
                                terminal_id,
                                data: BASE64.encode(&buffer[..read]),
                            });
                            if outgoing.send_blocking(event).is_err() {
                                break;
                            }
                        }
                    }
                }
                outgoing
                    .send_blocking(Outgoing::Event(Event::TerminalExit {
                        terminal_id,
                        code: None,
                    }))
                    .ok();
            })
            .context("spawning pty reader thread")?;

        Ok(Self {
            master: pair.master,
            writer,
            child,
        })
    }

    pub fn input(&mut self, data_base64: &str) -> Result<()> {
        let bytes = BASE64
            .decode(data_base64)
            .context("decoding terminal input")?;
        self.writer.write_all(&bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("resizing pty")
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if let Err(error) = self.child.kill() {
            log::debug!("terminal child already exited: {error}");
        }
    }
}
