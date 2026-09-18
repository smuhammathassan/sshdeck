//! Local PTY terminal session.
//!
//! Spawns a local shell (from `$SHELL` or `/bin/zsh`) in a real pseudo-terminal (PTY)
//! using `portable-pty`. IO is forwarded asynchronously over bounded `async_channel`s
//! matching `sshdeck_core::session::SessionEvent`.

use std::io::{Read, Write};

use async_channel::{Receiver, Sender};
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use sshdeck_core::session::SessionEvent;

enum LocalCommand {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    ChildEof,
    Shutdown,
}

pub struct LocalSession {
    commands: Sender<LocalCommand>,
    events: Receiver<SessionEvent>,
}

impl LocalSession {
    pub fn spawn(cols: u16, rows: u16) -> Result<Self, String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| {
            if cfg!(target_os = "windows") {
                std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".to_string())
            } else if cfg!(target_os = "macos") {
                "/bin/zsh".to_string()
            } else {
                "/bin/bash".to_string()
            }
        });

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("could not allocate PTY: {e}"))?;

        let mut cmd = CommandBuilder::new(&shell);
        if cfg!(unix) {
            cmd.args(["-l"]);
        }
        if let Ok(home) = std::env::var("HOME") {
            cmd.cwd(home);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("could not start {shell}: {e}"))?;
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("could not clone PTY reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("could not take PTY writer: {e}"))?;

        let (commands, cmd_rx) = async_channel::bounded(64);
        let (event_tx, events) = async_channel::bounded(256);

        // Notify connected immediately so the UI knows we are live
        let _ = event_tx.try_send(SessionEvent::Connected);

        let read_events = event_tx.clone();
        let read_cmd = commands.clone();
        std::thread::Builder::new()
            .name("local-pty-read".to_string())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                let mut reader = reader;
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            let _ = read_cmd.send_blocking(LocalCommand::ChildEof);
                            break;
                        }
                        Ok(n) => {
                            if read_events
                                .send_blocking(SessionEvent::Data(buf[..n].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = read_cmd.send_blocking(LocalCommand::ChildEof);
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("could not spawn reader thread: {e}"))?;

        let sup_events = event_tx;
        let master = pair.master;
        std::thread::Builder::new()
            .name("local-pty-supervise".to_string())
            .spawn(move || {
                supervise(child, master, writer, cmd_rx, sup_events);
            })
            .map_err(|e| format!("could not spawn supervisor thread: {e}"))?;

        Ok(Self { commands, events })
    }

    pub fn write(&self, data: &[u8]) -> Result<(), String> {
        self.commands
            .try_send(LocalCommand::Write(data.to_vec()))
            .map_err(|e| e.to_string())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.commands
            .try_send(LocalCommand::Resize { cols, rows })
            .map_err(|e| e.to_string())
    }

    pub fn events(&self) -> Receiver<SessionEvent> {
        self.events.clone()
    }

    #[allow(dead_code)]
    pub fn shutdown(&self) {
        let _ = self.commands.try_send(LocalCommand::Shutdown);
    }
}

impl Drop for LocalSession {
    fn drop(&mut self) {
        let _ = self.commands.try_send(LocalCommand::Shutdown);
        let _ = self.commands.close();
    }
}

fn kill_child(child: &mut (dyn Child + Send + Sync)) {
    let _ = ChildKiller::kill(child);
}

fn supervise(
    mut child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    mut writer: Box<dyn Write + Send>,
    cmd_rx: Receiver<LocalCommand>,
    event_tx: Sender<SessionEvent>,
) {
    loop {
        match cmd_rx.recv_blocking() {
            Ok(LocalCommand::Write(data)) => {
                if writer.write_all(&data).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            Ok(LocalCommand::Resize { cols, rows }) => {
                let _ = master.resize(PtySize {
                    rows: rows.max(1),
                    cols: cols.max(1),
                    pixel_width: 0,
                    pixel_height: 0,
                });
            }
            Ok(LocalCommand::ChildEof) => {
                let code = child.wait().ok().map(|s| s.exit_code() as i32);
                let _ = event_tx.send_blocking(SessionEvent::Closed(code));
                break;
            }
            Ok(LocalCommand::Shutdown) | Err(_) => {
                kill_child(&mut *child);
                let _ = child.wait();
                let _ = event_tx.send_blocking(SessionEvent::Closed(None));
                break;
            }
        }
    }
}
