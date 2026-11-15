use eframe::egui::Context;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    collections::VecDeque,
    env, fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        mpsc::{self, TryRecvError},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::config::SshHost;

/// Output chunks one session may handle per frame, within the app's shared budget.
const CHUNKS_PER_POLL: usize = 128;
/// Chunks buffered before the reader thread waits for the UI to catch up.
const CHANNEL_CAPACITY: usize = 64;
const COMMAND_MARKER: &[u8] = b"\x1b]1337;ShellHopperCommand=";
const COMMAND_MARKER_LIMIT: usize = 64 * 1024;
const COMMAND_HISTORY_LIMIT: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Connecting,
    Connected,
    Disconnected,
}

pub struct TerminalSession {
    pub id: u64,
    pub title: String,
    host: Option<SshHost>,
    state: SessionState,
    ctx: Context,
    ready_socket: Option<PathBuf>,
    pending_commands: VecDeque<String>,
    commands: Vec<String>,
    command_marker: Vec<u8>,
    parser: vt100::Parser,
    receiver: Option<mpsc::Receiver<Vec<u8>>>,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn Child + Send + Sync>>,
    size: PtySize,
    scroll_remainder: f32,
}

impl TerminalSession {
    pub fn local(id: u64, ctx: &Context) -> Result<Self, String> {
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let is_zsh = Path::new(&shell)
            .file_name()
            .is_some_and(|name| name == "zsh");
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        command.arg("-i");
        if is_zsh && let Some(zdotdir) = prepare_zsh_environment() {
            // The generated startup files forward to the user's own directory.
            if let Some(user_zdotdir) = env::var_os("ZDOTDIR") {
                command.env("SHELLHOPPER_USER_ZDOTDIR", user_zdotdir);
            }
            command.env("ZDOTDIR", zdotdir);
        }
        Self::spawn(
            id,
            "Local".into(),
            None,
            SessionState::Connected,
            command,
            None,
            ctx,
        )
    }

    pub fn ssh(id: u64, host: &SshHost, ctx: &Context) -> Result<Self, String> {
        let nonce = session_nonce();
        let ready_socket = PathBuf::from(format!(
            "/tmp/shellhopper-{}-{id}-{}.sock",
            std::process::id(),
            &nonce[..16]
        ));
        Self::spawn(
            id,
            host.name.clone(),
            Some(host.clone()),
            SessionState::Connecting,
            ssh_command(host, &ready_socket),
            Some(ready_socket),
            ctx,
        )
    }

    pub fn restored_ssh(id: u64, host: &SshHost, ctx: &Context) -> Self {
        let size = default_size();
        Self {
            id,
            title: host.name.clone(),
            host: Some(host.clone()),
            state: SessionState::Disconnected,
            ctx: ctx.clone(),
            ready_socket: None,
            pending_commands: VecDeque::new(),
            commands: Vec::new(),
            command_marker: Vec::new(),
            parser: vt100::Parser::new(size.rows, size.cols, 10_000),
            receiver: None,
            writer: None,
            master: None,
            child: None,
            size,
            scroll_remainder: 0.0,
        }
    }

    pub fn ssh_command(
        id: u64,
        host: &SshHost,
        remote_command: &str,
        ctx: &Context,
    ) -> Result<Self, String> {
        let mut session = Self::ssh(id, host, ctx)?;
        session.run_command(remote_command);
        Ok(session)
    }

    fn spawn(
        id: u64,
        title: String,
        host: Option<SshHost>,
        state: SessionState,
        mut command: CommandBuilder,
        ready_socket: Option<PathBuf>,
        ctx: &Context,
    ) -> Result<Self, String> {
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "ShellHopper");
        command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        let size = default_size();
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|error| format!("Could not open terminal: {error}"))?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| format!("Could not start terminal: {error}"))?;
        drop(pair.slave);

        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("Could not read terminal: {error}"))?;
        let writer = Arc::new(Mutex::new(
            pair.master
                .take_writer()
                .map_err(|error| format!("Could not write terminal: {error}"))?,
        ));
        // A bounded channel applies backpressure to the PTY instead of growing without limit.
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let repaint = ctx.clone();
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            while let Ok(count) = reader.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
                repaint.request_repaint();
            }
            // Wake the UI once more so it notices the closed session.
            drop(sender);
            repaint.request_repaint();
        });

        Ok(Self {
            id,
            title,
            host,
            state,
            ctx: ctx.clone(),
            ready_socket,
            pending_commands: VecDeque::new(),
            commands: Vec::new(),
            command_marker: Vec::new(),
            parser: vt100::Parser::new(size.rows, size.cols, 10_000),
            receiver: Some(receiver),
            writer: Some(writer),
            master: Some(pair.master),
            child: Some(child),
            size,
            scroll_remainder: 0.0,
        })
    }

    /// Processes waiting output, taking chunks from the frame's shared `budget`.
    /// Returns true when more may be waiting for the next frame.
    pub fn poll(&mut self, budget: &mut usize) -> bool {
        self.finish_connection_if_ready();
        for _ in 0..CHUNKS_PER_POLL {
            let Some(receiver) = self.receiver.as_ref() else {
                return false;
            };
            if *budget == 0 {
                return true;
            }
            match receiver.try_recv() {
                Ok(bytes) => {
                    *budget -= 1;
                    if self.host.is_none() {
                        extract_command_markers(
                            &mut self.command_marker,
                            &bytes,
                            &mut self.commands,
                        );
                    }
                    self.parser.process(&bytes);
                    self.finish_connection_if_ready();
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    self.state = SessionState::Disconnected;
                    // Never replay a queued command on some later reconnect.
                    self.pending_commands.clear();
                    self.receiver = None;
                    self.remove_ready_socket();
                    return false;
                }
            }
        }
        true
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn scroll_lines(&mut self, lines: isize) {
        let current = self.scrollback();
        let target = if lines.is_positive() {
            current.saturating_add(lines as usize)
        } else {
            current.saturating_sub(lines.unsigned_abs())
        };
        self.parser.screen_mut().set_scrollback(target);
    }

    pub fn scroll_by_points(&mut self, points: f32, row_height: f32) {
        self.scroll_remainder += points;
        let lines = (self.scroll_remainder / row_height).trunc() as isize;
        if lines != 0 {
            self.scroll_lines(lines);
            self.scroll_remainder -= lines as f32 * row_height;
        }
    }

    pub fn scroll_page(&mut self, older: bool) {
        let rows = self.screen().size().0.saturating_sub(1).max(1);
        self.scroll_lines(if older {
            isize::try_from(rows).unwrap_or(isize::MAX)
        } else {
            -isize::try_from(rows).unwrap_or(isize::MAX)
        });
    }

    pub fn scroll_to_live(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
        self.scroll_remainder = 0.0;
    }

    pub fn clear_screen(&mut self) {
        self.parser.process(b"\x1bc");
        self.scroll_remainder = 0.0;
        self.send(b"\x0c");
    }

    pub fn search_history(&mut self, query: &str, older: bool) -> bool {
        let query = query.trim();
        if query.is_empty() {
            return false;
        }

        let original = self.scrollback();
        let maximum = {
            let screen = self.parser.screen_mut();
            screen.set_scrollback(usize::MAX);
            let maximum = screen.scrollback();
            screen.set_scrollback(original);
            maximum
        };
        let rows = self.screen().size().0;

        if older {
            for offset in original.saturating_add(1)..=maximum {
                let screen = self.parser.screen_mut();
                screen.set_scrollback(offset);
                if screen
                    .rows(0, screen.size().1)
                    .next()
                    .is_some_and(|row| contains_search(&row, query))
                {
                    return true;
                }
            }
        } else {
            for offset in (0..original).rev() {
                let screen = self.parser.screen_mut();
                screen.set_scrollback(offset);
                if screen
                    .rows(0, screen.size().1)
                    .nth(usize::from(rows.saturating_sub(1)))
                    .is_some_and(|row| contains_search(&row, query))
                {
                    return true;
                }
            }
        }

        self.parser.screen_mut().set_scrollback(original);
        false
    }

    pub fn send(&self, bytes: &[u8]) {
        if let Some(writer) = &self.writer
            && let Ok(mut writer) = writer.lock()
        {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    pub fn is_host(&self, name: &str) -> bool {
        self.host.as_ref().is_some_and(|host| host.name == name)
    }

    pub fn is_remote(&self) -> bool {
        self.host.is_some()
    }

    pub fn host_name(&self) -> Option<&str> {
        self.host.as_ref().map(|host| host.name.as_str())
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn commands(&self) -> &[String] {
        &self.commands
    }

    pub fn upload_files(
        &self,
        expected_host: &str,
        paths: Vec<PathBuf>,
    ) -> Result<mpsc::Receiver<Result<usize, String>>, String> {
        if self.state != SessionState::Connected {
            return Err("File uploads require a connected SSH pane.".into());
        }
        let host = self
            .host
            .as_ref()
            .ok_or_else(|| "File uploads require an SSH pane.".to_owned())?;
        if host.name != expected_host {
            return Err("The SSH pane changed hosts before the upload started.".into());
        }
        if paths.is_empty() {
            return Err("Drop at least one file to upload.".into());
        }
        if let Some(path) = paths.iter().find(|path| !path.is_file()) {
            return Err(format!(
                "{} is not a regular file. Folder uploads are not supported yet.",
                path.display()
            ));
        }
        let socket = self
            .ready_socket
            .as_ref()
            .filter(|socket| socket.exists())
            .cloned()
            .ok_or_else(|| "The SSH connection is not ready for file uploads.".to_owned())?;
        let destination = scp_destination(host);
        let count = paths.len();
        let ctx = self.ctx.clone();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let result = scp_command(&socket, &destination, &paths)
                .output()
                .map_err(|error| format!("Could not start file upload: {error}"))
                .and_then(|output| {
                    if output.status.success() {
                        Ok(count)
                    } else {
                        let error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
                        Err(if error.is_empty() {
                            format!("File upload failed with {}.", output.status)
                        } else {
                            format!("File upload failed: {error}")
                        })
                    }
                });
            let _ = sender.send(result);
            ctx.request_repaint();
        });
        Ok(receiver)
    }

    /// Runs a command now, or queues it while the connection is still starting.
    /// A disconnected session drops it; reconnect first.
    pub fn run_command(&mut self, command: &str) {
        match self.state {
            SessionState::Connected => {
                if self.try_send(format!("{command}\n").as_bytes()) {
                    push_command(&mut self.commands, command.to_owned());
                } else {
                    self.state = SessionState::Disconnected;
                }
            }
            SessionState::Connecting => self.pending_commands.push_back(command.to_owned()),
            SessionState::Disconnected => {}
        }
    }

    /// Keeps an open tab pointing at a host that was edited or renamed. If the
    /// edit changes where SSH connects, the tab is disconnected so it can never
    /// be labelled as one server while still talking to another.
    pub fn replace_host(&mut self, original_name: &str, host: &SshHost) {
        let Some(current) = self
            .host
            .as_ref()
            .filter(|current| current.name == original_name)
        else {
            return;
        };
        if current.ssh_args(false) != host.ssh_args(false) {
            self.disconnect();
            self.commands.clear();
        }
        self.host = Some(host.clone());
    }

    /// Updates future reconnects without interrupting a live connection. The
    /// caller must first verify that both definitions identify the same server.
    pub fn replace_equivalent_host(&mut self, original_name: &str, host: &SshHost) {
        if self.is_host(original_name) {
            self.host = Some(host.clone());
        }
    }

    /// Ends the connection but keeps the tab and its scrollback.
    fn disconnect(&mut self) {
        self.terminate();
        self.remove_ready_socket();
        self.writer = None;
        self.master = None;
        self.receiver = None;
        self.pending_commands.clear();
        self.state = SessionState::Disconnected;
    }

    fn terminate(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // Reap the process off the UI thread so it does not linger as a zombie.
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    pub fn reconnect(&mut self) -> Result<(), String> {
        let host = self
            .host
            .clone()
            .ok_or_else(|| "Local terminals cannot reconnect over SSH.".to_owned())?;
        let title = self.title.clone();
        let commands = std::mem::take(&mut self.commands);
        let mut replacement = Self::ssh(self.id, &host, &self.ctx)?;
        replacement.title = title;
        replacement.commands = commands;
        *self = replacement;
        Ok(())
    }

    fn flush_pending_commands(&mut self) {
        while let Some(command) = self.pending_commands.front().cloned() {
            if !self.try_send(format!("{command}\n").as_bytes()) {
                self.state = SessionState::Disconnected;
                break;
            }
            self.pending_commands.pop_front();
            push_command(&mut self.commands, command);
        }
    }

    fn finish_connection_if_ready(&mut self) {
        if self.state == SessionState::Connecting
            && self
                .ready_socket
                .as_ref()
                .is_some_and(|socket| socket.exists())
        {
            self.state = SessionState::Connected;
            self.flush_pending_commands();
        }
    }

    fn remove_ready_socket(&mut self) {
        if let Some(socket) = self.ready_socket.take() {
            let _ = fs::remove_file(socket);
        }
    }

    fn try_send(&self, bytes: &[u8]) -> bool {
        let Some(writer) = &self.writer else {
            return false;
        };
        let Ok(mut writer) = writer.lock() else {
            return false;
        };
        writer.write_all(bytes).and_then(|_| writer.flush()).is_ok()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if rows < 2 || cols < 20 || (rows == self.size.rows && cols == self.size.cols) {
            return;
        }
        self.size.rows = rows;
        self.size.cols = cols;
        if let Some(master) = &self.master {
            let _ = master.resize(self.size);
        }
        self.parser.screen_mut().set_size(rows, cols);
    }
}

fn default_size() -> PtySize {
    PtySize {
        rows: 36,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn session_nonce() -> String {
    let mut bytes = [0_u8; 16];
    if fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_err()
    {
        bytes = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
            .to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ssh_command(host: &SshHost, ready_socket: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new("/usr/bin/ssh");
    command.arg("-tt");
    command.args([
        "-o",
        "ControlMaster=yes",
        "-o",
        "ControlPersist=no",
        "-o",
        &format!("ControlPath={}", ready_socket.display()),
    ]);
    for argument in host.ssh_args(false) {
        command.arg(argument);
    }
    command
}

fn scp_destination(host: &SshHost) -> String {
    if !host.alias.is_empty() {
        return format!("{}:", host.alias);
    }
    let address = if host.address.is_empty() {
        host.name.clone()
    } else if host.address.contains(':')
        && !(host.address.starts_with('[') && host.address.ends_with(']'))
    {
        format!("[{}]", host.address)
    } else {
        host.address.clone()
    };
    if host.user.is_empty() {
        format!("{address}:")
    } else {
        format!("{}@{address}:", host.user)
    }
}

fn scp_command(socket: &Path, destination: &str, paths: &[PathBuf]) -> Command {
    let mut command = Command::new("/usr/bin/scp");
    // Reuse the live SSH master so aliases, jump hosts, and authentication stay exact.
    command
        .arg("-q")
        .arg("-o")
        .arg(format!("ControlPath={}", socket.display()))
        .arg("--");
    command.args(paths);
    command.arg(destination);
    command
}

fn prepare_zsh_environment() -> Option<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME")?);
    let directory = home
        .join("Library")
        .join("Application Support")
        .join("ShellHopper")
        .join("zsh");
    fs::create_dir_all(&directory).ok()?;

    for name in [".zshenv", ".zprofile"] {
        write_if_changed(&directory.join(name), &forwarding_file(name)).ok()?;
    }
    // .zlogin is the last startup file, so it hands ZDOTDIR back to the user. From
    // then on history, .zlogout and anything a tool appends go to their own files.
    let zlogin = format!(
        "{}ZDOTDIR=$SHELLHOPPER_USER_ZDOTDIR\n[[ $ZDOTDIR == \"$HOME\" ]] && unset ZDOTDIR\nunset SHELLHOPPER_USER_ZDOTDIR\n",
        forwarding_file(".zlogin")
    );
    write_if_changed(&directory.join(".zlogin"), &zlogin).ok()?;
    let _ = fs::remove_file(directory.join(".zlogout"));
    let zshrc = format!(
        "{}\nsource \"$ZDOTDIR/shellhopper-highlight.zsh\"\n",
        forwarding_file(".zshrc")
    );
    write_if_changed(&directory.join(".zshrc"), &zshrc).ok()?;
    write_if_changed(
        &directory.join("shellhopper-highlight.zsh"),
        include_str!("shellhopper-highlight.zsh"),
    )
    .ok()?;
    Some(directory)
}

fn forwarding_file(name: &str) -> String {
    // Sources the user's own file from their ZDOTDIR (which an earlier file may
    // have changed), and points the system default HISTFILE back at it.
    format!(
        "_shellhopper_zdotdir=$ZDOTDIR\n\
         ZDOTDIR=${{SHELLHOPPER_USER_ZDOTDIR:-$HOME}}\n\
         [[ $HISTFILE == \"$_shellhopper_zdotdir/.zsh_history\" ]] && HISTFILE=$ZDOTDIR/.zsh_history\n\
         [[ -r \"$ZDOTDIR/{name}\" ]] && source \"$ZDOTDIR/{name}\"\n\
         SHELLHOPPER_USER_ZDOTDIR=$ZDOTDIR\n\
         ZDOTDIR=$_shellhopper_zdotdir\n\
         unset _shellhopper_zdotdir\n"
    )
}

fn write_if_changed(path: &Path, contents: &str) -> std::io::Result<()> {
    if !matches!(fs::read_to_string(path), Ok(existing) if existing == contents) {
        fs::write(path, contents)?;
    }
    Ok(())
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.terminate();
        self.remove_ready_socket();
    }
}

fn contains_search(text: &str, query: &str) -> bool {
    text.to_ascii_lowercase()
        .contains(&query.to_ascii_lowercase())
}

fn extract_command_markers(buffer: &mut Vec<u8>, bytes: &[u8], commands: &mut Vec<String>) {
    buffer.extend_from_slice(bytes);
    loop {
        let Some(start) = find_bytes(buffer, COMMAND_MARKER) else {
            let keep = (1..COMMAND_MARKER.len().min(buffer.len() + 1))
                .rev()
                .find(|length| buffer.ends_with(&COMMAND_MARKER[..*length]))
                .unwrap_or(0);
            buffer.drain(..buffer.len().saturating_sub(keep));
            return;
        };
        buffer.drain(..start);
        let payload_start = COMMAND_MARKER.len();
        let Some(end) = buffer[payload_start..]
            .iter()
            .position(|byte| *byte == b'\x07')
            .map(|end| payload_start + end)
        else {
            // shortcut: commands over 64 KiB are ignored, raise the cap if legitimate use appears.
            if buffer.len() > COMMAND_MARKER_LIMIT {
                buffer.clear();
            }
            return;
        };
        let command = decode_command_marker(&buffer[payload_start..end]);
        buffer.drain(..=end);
        if let Some(command) = command.filter(|command| !command.is_empty()) {
            push_command(commands, command);
        }
    }
}

fn push_command(commands: &mut Vec<String>, command: String) {
    if commands.len() >= COMMAND_HISTORY_LIMIT {
        commands.drain(..=commands.len() - COMMAND_HISTORY_LIMIT);
    }
    commands.push(command);
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn decode_command_marker(payload: &[u8]) -> Option<String> {
    let mut decoded = String::new();
    let mut chars = std::str::from_utf8(payload).ok()?.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some('e') => decoded.push('\x1b'),
            Some('a') => decoded.push('\x07'),
            Some('\\') => decoded.push('\\'),
            Some(character) => {
                decoded.push('\\');
                decoded.push(character);
            }
            None => decoded.push('\\'),
        }
    }
    Some(decoded)
}

#[cfg(test)]
mod tests {
    use super::{
        SessionState, TerminalSession, extract_command_markers, scp_command, scp_destination,
        ssh_command,
    };
    use crate::config::SshHost;
    use eframe::egui::Context;
    use std::{
        env, fs,
        path::Path,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn ssh_starts_a_plain_login_session() {
        let host = SshHost {
            name: "stage-web".into(),
            address: "10.0.0.9".into(),
            port: 22,
            ..Default::default()
        };
        let command = ssh_command(&host, Path::new("/tmp/shellhopper-test.sock"));
        let arguments: Vec<_> = command
            .get_argv()
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();

        assert!(
            arguments
                .iter()
                .any(|argument| argument == "ControlMaster=yes")
        );
        assert_eq!(
            arguments.last().map(|argument| argument.as_ref()),
            Some("10.0.0.9")
        );
    }

    #[test]
    fn scp_reuses_the_live_connection_without_shell_parsing() {
        let host = SshHost {
            name: "prod web".into(),
            alias: "prod-alias".into(),
            ..Default::default()
        };
        let command = scp_command(
            Path::new("/tmp/shellhopper socket"),
            &scp_destination(&host),
            &["/tmp/report one.txt".into(), "/tmp/-report-two.txt".into()],
        );
        let arguments: Vec<_> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            arguments,
            [
                "-q",
                "-o",
                "ControlPath=/tmp/shellhopper socket",
                "--",
                "/tmp/report one.txt",
                "/tmp/-report-two.txt",
                "prod-alias:",
            ]
        );
    }

    #[test]
    fn command_markers_survive_chunks_and_decode_shell_text() {
        let mut buffer = Vec::new();
        let mut commands = Vec::new();

        extract_command_markers(
            &mut buffer,
            b"prompt\x1b]1337;ShellHopperCom",
            &mut commands,
        );
        extract_command_markers(
            &mut buffer,
            b"mand=printf 'one\\\\ntwo'\\nnext\x07output",
            &mut commands,
        );

        assert_eq!(commands, ["printf 'one\\ntwo'\nnext"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn editing_a_host_keeps_or_replaces_tab_metadata() {
        let host = SshHost {
            name: "stage-web".into(),
            address: "10.0.0.1".into(),
            port: 22,
            ..Default::default()
        };
        let mut session = TerminalSession::restored_ssh(1, &host, &Context::default());
        let moved = SshHost {
            name: "stage".into(),
            address: "10.0.0.2".into(),
            ..host.clone()
        };

        session.replace_host("other", &moved);
        assert_eq!(session.host_name(), Some("stage-web"));
        session.replace_host("stage-web", &moved);
        assert_eq!(session.host_name(), Some("stage"));
        assert_eq!(session.state(), SessionState::Disconnected);
    }

    #[test]
    #[ignore = "requires the opt-in loopback Docker fixture"]
    fn ssh_fixture_queues_reuses_and_reconnects() {
        assert_eq!(env::var("SHELLHOPPER_RUN_SSH_FIXTURE").as_deref(), Ok("1"));
        let config_path = env::var("SHELLHOPPER_SSH_CONFIG")
            .expect("run tests/fixtures/ssh/start.sh and export SHELLHOPPER_SSH_CONFIG");
        assert!(Path::new(&config_path).is_absolute());
        let config = fs::read_to_string(&config_path).expect("read fixture SSH config");
        assert!(config.contains("HostName 127.0.0.1"));
        assert!(config.contains("UserKnownHostsFile"));

        let host = SshHost {
            name: "shellhopper-fixture".into(),
            address: "shellhopper-fixture".into(),
            group: "Fixture".into(),
            port: 22,
            ..Default::default()
        };
        let queued = "shellhopper-fixture-queued";
        let mut session =
            TerminalSession::ssh_command(700, &host, &encoded_print(queued), &Context::default())
                .expect("start fixture SSH session");
        assert_eq!(session.state(), SessionState::Connecting);
        assert_eq!(session.pending_commands.len(), 1);
        wait_for(&mut session, queued, SessionState::Connected);

        let reused = "shellhopper-fixture-reused";
        session.run_command(&encoded_print(reused));
        wait_for(&mut session, reused, SessionState::Connected);
        assert_eq!(session.id, 700);

        session.run_command("exit");
        wait_for_state(&mut session, SessionState::Disconnected);
        session.reconnect().expect("reconnect fixture SSH session");
        let reconnected = "shellhopper-fixture-reconnected";
        session.run_command(&encoded_print(reconnected));
        assert_eq!(session.state(), SessionState::Connecting);
        assert_eq!(session.pending_commands.len(), 1);
        wait_for(&mut session, reconnected, SessionState::Connected);
        assert_eq!(session.id, 700);
    }

    fn encoded_print(text: &str) -> String {
        let octal: String = text.bytes().map(|byte| format!("\\{byte:03o}")).collect();
        format!("printf '{octal}\\n'")
    }

    fn wait_for(session: &mut TerminalSession, text: &str, state: SessionState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut budget = usize::MAX;
            session.poll(&mut budget);
            if session.state() == state && session.screen().contents().contains(text) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "fixture output did not contain {text:?}; state={:?}, screen={:?}",
            session.state(),
            session.screen().contents()
        );
    }

    fn wait_for_state(session: &mut TerminalSession, state: SessionState) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let mut budget = usize::MAX;
            session.poll(&mut budget);
            if session.state() == state {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("fixture session did not reach {state:?}");
    }
}
