use eframe::egui::{
    self, Align, Align2, Color32, FontId, Key, Layout, Margin, RichText, Sense, Stroke,
    text::{LayoutJob, TextFormat},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, TryRecvError},
    thread,
};

#[cfg(target_os = "macos")]
use crate::menu_bar::{MenuBar, MenuBarAction};
#[cfg(target_os = "macos")]
use crate::updater::{UpdateNotice, Updater};
use crate::{
    config::{
        AppConfig, ImportKind, SavedCommand, SavedSplitDirection, SavedTab, SshHost,
        import_ssh_config, resolve_ssh_alias,
    },
    terminal::{SessionState, TerminalSession},
    theme::{
        self, HostRowAction, MUTED, RED, SIDEBAR, TERM_FONT_SIZE, TERM_LINE_HEIGHT, TERMINAL,
        TITLEBAR_HEIGHT,
    },
};
#[cfg(target_os = "macos")]
use objc2_app_kit::NSApplication;
#[cfg(target_os = "macos")]
use objc2_foundation::MainThreadMarker;

/// Output chunks (8 KB each) parsed per frame across every session.
const FRAME_CHUNK_BUDGET: usize = 256;

pub struct ShellHopperApp {
    ctx: egui::Context,
    config: AppConfig,
    /// False when the settings file could not be read, so it is never overwritten.
    config_writable: bool,
    import: Option<(ImportKind, mpsc::Receiver<Vec<SshHost>>)>,
    alias_check: Option<(String, mpsc::Receiver<Result<SshHost, String>>)>,
    sessions: Vec<TerminalSession>,
    splits: BTreeMap<u64, PaneLayout>,
    active_session: Option<u64>,
    terminal_focus: Option<u64>,
    terminal_selection: Option<TerminalSelection>,
    dragged_tab: Option<u64>,
    next_session_id: u64,
    next_split_id: u64,
    /// Which session gets first call on the next frame's output budget.
    poll_start: usize,
    selected_hosts: BTreeSet<String>,
    search: String,
    terminal_search: String,
    terminal_search_open: bool,
    terminal_search_focus: bool,
    terminal_search_status: Option<String>,
    command_history_open: bool,
    command_history_search: String,
    command_history_focus: bool,
    command: String,
    editor: Option<HostEditor>,
    saved_commands_editor: Option<Vec<SavedCommand>>,
    rename_tab: Option<TabRename>,
    new_group: Option<String>,
    move_group: String,
    message: Option<String>,
    pending_upload: Option<PendingUpload>,
    upload: Option<Upload>,
    file_drop_target: Option<u64>,
    closed_tabs: Vec<ClosedTab>,
    show_quit_confirmation: bool,
    #[cfg(target_os = "macos")]
    show_update_confirmation: bool,
    quit_confirmed: bool,
    #[cfg(target_os = "macos")]
    menu_bar: MenuBar,
    #[cfg(target_os = "macos")]
    updater: Updater,
    #[cfg(target_os = "macos")]
    native_window_configured: bool,
}

#[derive(Default)]
struct HostEditor {
    original_name: Option<String>,
    host: SshHost,
    confirm_delete: bool,
}

struct TabRename {
    id: u64,
    title: String,
    focus: bool,
}

#[derive(Clone)]
struct DraggedGroup(String);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TerminalPoint {
    row: usize,
    col: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TerminalSelection {
    session_id: u64,
    anchor: TerminalPoint,
    head: TerminalPoint,
    cols: u16,
    dragging: bool,
}

impl TerminalSelection {
    fn range(self) -> (TerminalPoint, TerminalPoint) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    fn has_text(self) -> bool {
        self.anchor != self.head
    }

    fn contains(self, row: usize, col: u16) -> bool {
        let (start, end) = self.range();
        let cell = TerminalPoint { row, col };
        start <= cell && cell < end
    }
}

struct ClosedTab {
    descriptor: SavedTab,
    title: String,
    index: usize,
}

struct PendingUpload {
    session_id: u64,
    host: String,
    paths: Vec<PathBuf>,
}

struct Upload {
    host: String,
    receiver: mpsc::Receiver<Result<usize, String>>,
}

#[derive(Clone)]
enum PaneLayout {
    Leaf(u64),
    Split {
        id: u64,
        direction: SavedSplitDirection,
        ratio: f32,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    fn first_leaf(&self) -> u64 {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { first, .. } => first.first_leaf(),
        }
    }

    fn contains(&self, target: u64) -> bool {
        match self {
            Self::Leaf(id) => *id == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn split_leaf(
        &mut self,
        target: u64,
        new_leaf: u64,
        split_id: u64,
        direction: SavedSplitDirection,
    ) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Split {
                    id: split_id,
                    direction,
                    ratio: 0.5,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new_leaf)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(target, new_leaf, split_id, direction)
                    || second.split_leaf(target, new_leaf, split_id, direction)
            }
        }
    }

    fn remove_leaf(self, target: u64) -> (Option<Self>, Option<u64>) {
        match self {
            Self::Leaf(id) if id == target => (None, None),
            leaf @ Self::Leaf(_) => (Some(leaf), None),
            Self::Split {
                id,
                direction,
                ratio,
                first,
                second,
            } => {
                if first.contains(target) {
                    let (first, focus) = first.remove_leaf(target);
                    match first {
                        Some(first) => (
                            Some(Self::Split {
                                id,
                                direction,
                                ratio,
                                first: Box::new(first),
                                second,
                            }),
                            focus,
                        ),
                        None => {
                            let focus = second.first_leaf();
                            (Some(*second), Some(focus))
                        }
                    }
                } else if second.contains(target) {
                    let (second, focus) = second.remove_leaf(target);
                    match second {
                        Some(second) => (
                            Some(Self::Split {
                                id,
                                direction,
                                ratio,
                                first,
                                second: Box::new(second),
                            }),
                            focus,
                        ),
                        None => {
                            let focus = first.first_leaf();
                            (Some(*first), Some(focus))
                        }
                    }
                } else {
                    (
                        Some(Self::Split {
                            id,
                            direction,
                            ratio,
                            first,
                            second,
                        }),
                        None,
                    )
                }
            }
        }
    }

    fn collect_leaves(&self, leaves: &mut BTreeSet<u64>) {
        match self {
            Self::Leaf(id) => {
                leaves.insert(*id);
            }
            Self::Split { first, second, .. } => {
                first.collect_leaves(leaves);
                second.collect_leaves(leaves);
            }
        }
    }

    fn adjust_ratio(&mut self, target: u64, adjustment: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                id,
                ratio,
                first,
                second,
                ..
            } => {
                if *id == target {
                    *ratio = (*ratio + adjustment).clamp(0.2, 0.8);
                    true
                } else {
                    first.adjust_ratio(target, adjustment)
                        || second.adjust_ratio(target, adjustment)
                }
            }
        }
    }
}

enum TabContextAction {
    Reconnect,
    SplitRight,
    SplitDown,
    Close,
    CloseLeft,
    CloseRight,
    KeepOnly,
}

#[derive(Clone, Copy)]
enum TerminalSearchDirection {
    Older,
    Newer,
}

fn tab_status(session: &TerminalSession) -> (Color32, &'static str) {
    if !session.is_remote() {
        return (theme::FAINT, "Local terminal");
    }
    match session.state() {
        SessionState::Connecting => (Color32::from_rgb(218, 169, 76), "Connecting"),
        SessionState::Connected => (Color32::from_rgb(76, 175, 121), "Connected"),
        SessionState::Disconnected => (RED, "Disconnected"),
    }
}

#[cfg(target_os = "macos")]
fn disable_native_titlebar_dragging() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApplication::sharedApplication(mtm);
    let window = app.keyWindow().or_else(|| unsafe { app.mainWindow() });
    if let Some(window) = window {
        window.setMovable(false);
        true
    } else {
        false
    }
}

fn reorder_session(
    sessions: &mut Vec<TerminalSession>,
    dragged: u64,
    target: u64,
    after: bool,
) -> bool {
    let Some(from) = sessions.iter().position(|session| session.id == dragged) else {
        return false;
    };
    let Some(to) = sessions.iter().position(|session| session.id == target) else {
        return false;
    };
    move_item(sessions, from, to, after)
}

fn move_item<T>(items: &mut Vec<T>, from: usize, target: usize, after: bool) -> bool {
    if from == target || from >= items.len() || target >= items.len() {
        return false;
    }
    let item = items.remove(from);
    let mut destination = target + usize::from(after);
    if from < destination {
        destination -= 1;
    }
    items.insert(destination.min(items.len()), item);
    true
}

fn adjacent_tab(tab_ids: &[u64], active: Option<u64>, offset: isize) -> Option<u64> {
    let active = active.and_then(|id| tab_ids.iter().position(|candidate| *candidate == id));
    let index = active.unwrap_or(if offset < 0 {
        0
    } else {
        tab_ids.len().saturating_sub(1)
    });
    (!tab_ids.is_empty()).then(|| {
        let next = (index as isize + offset).rem_euclid(tab_ids.len() as isize) as usize;
        tab_ids[next]
    })
}

impl ShellHopperApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);

        let (config, startup_import, message) = match AppConfig::load() {
            Ok((config, import)) => (config, import, None),
            Err(error) => (
                AppConfig::default(),
                None,
                Some(format!(
                    "{error}\n\nNothing will be saved until the file is fixed and ShellHopper is restarted."
                )),
            ),
        };
        let move_group = config
            .groups
            .first()
            .cloned()
            .unwrap_or_else(|| "SSH Config".into());
        #[cfg(target_os = "macos")]
        let menu_bar = MenuBar::new(&config, &cc.egui_ctx);
        #[cfg(target_os = "macos")]
        let mut updater = Updater::new(&cc.egui_ctx);
        #[cfg(target_os = "macos")]
        updater.check(false);
        let mut app = Self {
            ctx: cc.egui_ctx.clone(),
            config_writable: message.is_none(),
            config,
            import: None,
            alias_check: None,
            sessions: Vec::new(),
            splits: BTreeMap::new(),
            active_session: None,
            terminal_focus: None,
            terminal_selection: None,
            dragged_tab: None,
            next_session_id: 1,
            next_split_id: 1,
            poll_start: 0,
            selected_hosts: BTreeSet::new(),
            search: String::new(),
            terminal_search: String::new(),
            terminal_search_open: false,
            terminal_search_focus: false,
            terminal_search_status: None,
            command_history_open: false,
            command_history_search: String::new(),
            command_history_focus: false,
            command: String::new(),
            editor: None,
            saved_commands_editor: None,
            rename_tab: None,
            new_group: None,
            move_group,
            message,
            pending_upload: None,
            upload: None,
            file_drop_target: None,
            closed_tabs: Vec::new(),
            show_quit_confirmation: false,
            #[cfg(target_os = "macos")]
            show_update_confirmation: false,
            quit_confirmed: false,
            #[cfg(target_os = "macos")]
            menu_bar,
            #[cfg(target_os = "macos")]
            updater,
            #[cfg(target_os = "macos")]
            native_window_configured: false,
        };
        app.restore_saved_tabs();
        if let Some(kind) = startup_import {
            app.start_import(kind);
        }
        app
    }

    /// Reads `~/.ssh/config` on a background thread; `poll` applies the result.
    fn start_import(&mut self, kind: ImportKind) {
        if self.import.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = sender.send(import_ssh_config());
            ctx.request_repaint();
        });
        self.import = Some((kind, receiver));
    }

    fn start_alias_check(&mut self, name: String) {
        if self.alias_check.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        let ctx = self.ctx.clone();
        let alias = name.clone();
        std::thread::spawn(move || {
            let _ = sender.send(resolve_ssh_alias(&alias));
            ctx.request_repaint();
        });
        self.alias_check = Some((name, receiver));
    }

    fn stop_using_alias(&mut self, name: &str) {
        let Some(host) = self.config.hosts.iter_mut().find(|host| host.name == name) else {
            return;
        };
        host.alias.clear();
        let host = host.clone();
        for session in &mut self.sessions {
            session.replace_equivalent_host(name, &host);
        }
        self.save_config();
        self.message = Some(format!(
            "{name} now uses its saved address, user, port, and key. Live sessions were left open."
        ));
    }

    fn restore_saved_tabs(&mut self) {
        let Some(tabs) = self.config.open_tabs.clone() else {
            self.open_local();
            return;
        };
        let mut restored_ids = Vec::with_capacity(tabs.len());
        for tab in tabs {
            match self.restore_tab(&tab, false) {
                Ok(id) => restored_ids.push(Some(id)),
                Err(error) => {
                    restored_ids.push(None);
                    self.message = Some(error);
                }
            }
        }
        self.active_session = self
            .config
            .active_tab
            .and_then(|index| restored_ids.get(index).copied().flatten())
            .or_else(|| self.sessions.last().map(|session| session.id));
    }

    fn restore_tab(&mut self, tab: &SavedTab, reconnect: bool) -> Result<u64, String> {
        let (layout, sessions) = self.restore_layout(tab, reconnect)?;
        let root = layout.first_leaf();
        self.sessions.extend(sessions);
        if matches!(layout, PaneLayout::Split { .. }) {
            self.splits.insert(root, layout);
        }
        Ok(root)
    }

    fn restore_layout(
        &mut self,
        pane: &SavedTab,
        reconnect: bool,
    ) -> Result<(PaneLayout, Vec<TerminalSession>), String> {
        match pane {
            SavedTab::Split {
                direction,
                ratio,
                first,
                second,
            } => {
                let (first, mut sessions) = self.restore_layout(first, reconnect)?;
                let (second, second_sessions) = self.restore_layout(second, reconnect)?;
                sessions.extend(second_sessions);
                let id = self.take_split_id();
                Ok((
                    PaneLayout::Split {
                        id,
                        direction: *direction,
                        ratio: (f32::from(*ratio) / 1_000.0).clamp(0.2, 0.8),
                        first: Box::new(first),
                        second: Box::new(second),
                    },
                    sessions,
                ))
            }
            SavedTab::Local => {
                let id = self.take_session_id();
                let session = TerminalSession::local(id, &self.ctx)?;
                Ok((PaneLayout::Leaf(id), vec![session]))
            }
            SavedTab::Ssh { host } => {
                let id = self.take_session_id();
                let host = self
                    .config
                    .hosts
                    .iter()
                    .find(|candidate| candidate.name == *host)
                    .ok_or_else(|| {
                        format!("Could not restore {host}: the host is no longer in the sidebar.")
                    })?;
                let session = if reconnect {
                    TerminalSession::ssh(id, host, &self.ctx)
                } else {
                    Ok(TerminalSession::restored_ssh(id, host, &self.ctx))
                }?;
                Ok((PaneLayout::Leaf(id), vec![session]))
            }
        }
    }

    fn open_local(&mut self) {
        let id = self.take_session_id();
        match TerminalSession::local(id, &self.ctx) {
            Ok(session) => {
                self.sessions.push(session);
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn open_host(&mut self, host: SshHost, focus: bool) {
        let id = self.take_session_id();
        match TerminalSession::ssh(id, &host, &self.ctx) {
            Ok(session) => {
                self.sessions.push(session);
                self.active_session = Some(id);
                if focus {
                    self.terminal_focus = Some(id);
                }
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn connect_pane(&mut self, id: u64, host: SshHost) {
        match TerminalSession::ssh(id, &host, &self.ctx) {
            Ok(session) => {
                let Some(index) = self.sessions.iter().position(|session| session.id == id) else {
                    return;
                };
                self.sessions[index] = session;
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
                self.persist_tab_layout();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn open_host_command(&mut self, host: SshHost, command: &str) {
        let id = self.take_session_id();
        match TerminalSession::ssh_command(id, &host, command, &self.ctx) {
            Ok(session) => {
                self.sessions.push(session);
                self.active_session = Some(id);
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn split_active(&mut self, direction: SavedSplitDirection) {
        let Some(active) = self.active_session else {
            return;
        };
        let root = self.tab_root(active);
        let host = self
            .sessions
            .iter()
            .find(|session| session.id == active)
            .and_then(TerminalSession::host_name)
            .and_then(|name| self.config.hosts.iter().find(|host| host.name == name))
            .cloned();
        let id = self.take_session_id();
        let session = host.map_or_else(
            || TerminalSession::local(id, &self.ctx),
            |host| TerminalSession::ssh(id, &host, &self.ctx),
        );
        match session {
            Ok(session) => {
                let split_id = self.take_split_id();
                let mut layout = self.splits.remove(&root).unwrap_or(PaneLayout::Leaf(root));
                if !layout.split_leaf(active, id, split_id, direction) {
                    self.message = Some("Could not find the active terminal pane.".into());
                    return;
                }
                self.sessions.push(session);
                self.splits.insert(root, layout);
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
                self.persist_tab_layout();
            }
            Err(error) => self.message = Some(error),
        }
    }

    fn close_pane_or_tab(&mut self, id: u64) {
        let root = self.tab_root(id);
        let Some(layout) = self.splits.remove(&root) else {
            self.close_session(root);
            return;
        };
        if !layout.contains(id) {
            self.splits.insert(root, layout);
            return;
        }
        let root_index = self.sessions.iter().position(|session| session.id == root);
        let (layout, focus) = layout.remove_leaf(id);
        self.sessions.retain(|session| session.id != id);
        if self
            .terminal_selection
            .is_some_and(|selection| selection.session_id == id)
        {
            self.terminal_selection = None;
        }
        let Some(layout) = layout else {
            self.close_session(root);
            return;
        };
        let new_root = layout.first_leaf();
        if new_root != root
            && let (Some(root_index), Some(keep_index)) = (
                root_index,
                self.sessions
                    .iter()
                    .position(|session| session.id == new_root),
            )
        {
            let session = self.sessions.remove(keep_index);
            self.sessions
                .insert(root_index.min(self.sessions.len()), session);
        }
        if matches!(layout, PaneLayout::Split { .. }) {
            self.splits.insert(new_root, layout);
        }
        let focus = focus.unwrap_or(new_root);
        self.active_session = Some(focus);
        self.terminal_focus = Some(focus);
        self.persist_tab_layout();
    }

    fn take_session_id(&mut self) -> u64 {
        let id = self.next_session_id;
        self.next_session_id += 1;
        id
    }

    fn take_split_id(&mut self) -> u64 {
        let id = self.next_split_id;
        self.next_split_id += 1;
        id
    }

    fn tab_root(&self, id: u64) -> u64 {
        self.splits
            .iter()
            .find_map(|(root, layout)| layout.contains(id).then_some(*root))
            .unwrap_or(id)
    }

    fn tab_ids(&self) -> Vec<u64> {
        self.sessions
            .iter()
            .filter(|session| self.tab_root(session.id) == session.id)
            .map(|session| session.id)
            .collect()
    }

    fn saved_layout(&self, root: u64) -> Option<SavedTab> {
        self.splits.get(&root).map_or_else(
            || {
                self.sessions
                    .iter()
                    .find(|session| session.id == root)
                    .map(saved_tab)
            },
            |layout| saved_pane_layout(layout, &self.sessions),
        )
    }

    fn close_session(&mut self, id: u64) {
        let root = self.tab_root(id);
        let tab_ids = self.tab_ids();
        let Some(index) = tab_ids.iter().position(|candidate| *candidate == root) else {
            return;
        };
        let Some(descriptor) = self.saved_layout(root) else {
            return;
        };
        let title = self
            .sessions
            .iter()
            .find(|session| session.id == root)
            .map(|session| session.title.clone())
            .unwrap_or_default();
        let active_in_tab = self.active_session.is_some_and(|active| {
            active == root
                || self
                    .splits
                    .get(&root)
                    .is_some_and(|layout| layout.contains(active))
        });
        let mut pane_ids = BTreeSet::from([root]);
        if let Some(layout) = self.splits.remove(&root) {
            layout.collect_leaves(&mut pane_ids);
        }
        self.sessions
            .retain(|session| !pane_ids.contains(&session.id));
        if self
            .terminal_selection
            .is_some_and(|selection| pane_ids.contains(&selection.session_id))
        {
            self.terminal_selection = None;
        }
        self.closed_tabs.push(ClosedTab {
            descriptor,
            title,
            index,
        });
        if active_in_tab {
            let remaining = self.tab_ids();
            self.active_session = remaining.get(index).or_else(|| remaining.last()).copied();
        }
    }

    fn restore_closed_tab(&mut self) {
        let Some(closed) = self.closed_tabs.pop() else {
            return;
        };
        match self.restore_tab(&closed.descriptor, true) {
            Ok(id) => {
                if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
                    session.title.clone_from(&closed.title);
                }
                let roots = self.tab_ids();
                if let Some(target) = roots.get(closed.index).copied()
                    && target != id
                {
                    reorder_session(&mut self.sessions, id, target, false);
                }
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
            }
            Err(error) => {
                self.closed_tabs.push(closed);
                self.message = Some(error);
            }
        }
    }

    fn persist_tab_layout(&mut self) {
        let roots = self.tab_ids();
        let tabs: Vec<_> = roots
            .iter()
            .filter_map(|root| self.saved_layout(*root))
            .collect();
        let active_tab = self
            .active_session
            .map(|id| self.tab_root(id))
            .and_then(|active| roots.iter().position(|root| *root == active));
        if self.config.open_tabs.as_ref() == Some(&tabs) && self.config.active_tab == active_tab {
            return;
        }
        self.config.open_tabs = Some(tabs);
        self.config.active_tab = active_tab;
        self.save_config();
    }

    fn live_ssh_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|session| {
                session.host_name().is_some()
                    && matches!(
                        session.state(),
                        SessionState::Connecting | SessionState::Connected
                    )
            })
            .count()
    }

    fn save_config(&mut self) {
        if !self.config_writable {
            return;
        }
        if let Err(error) = self.config.save() {
            self.message = Some(error);
        }
        #[cfg(target_os = "macos")]
        self.menu_bar.refresh(&self.config);
    }

    fn connect_selected(&mut self) {
        let hosts: Vec<_> = self
            .config
            .hosts
            .iter()
            .filter(|host| self.selected_hosts.contains(&host.name))
            .cloned()
            .collect();
        let focus = hosts.len() == 1;
        for host in hosts {
            self.open_host(host, focus);
        }
    }

    fn run_selected(&mut self) {
        let command = self.command.trim().to_owned();
        if command.is_empty() {
            self.message = Some("Enter a command first.".into());
            return;
        }
        let hosts: Vec<_> = self
            .config
            .hosts
            .iter()
            .filter(|host| self.selected_hosts.contains(&host.name))
            .cloned()
            .collect();
        if hosts.is_empty() {
            self.message = Some("Select at least one host.".into());
            return;
        }

        let mut skipped = Vec::new();
        for host in hosts {
            let existing = self
                .sessions
                .iter()
                .rposition(|session| {
                    session.is_host(&host.name) && session.state() != SessionState::Disconnected
                })
                .or_else(|| {
                    self.sessions
                        .iter()
                        .rposition(|session| session.is_host(&host.name))
                });
            if let Some(index) = existing {
                let session = &mut self.sessions[index];
                // Typing a script into vim, less or another full-screen program would
                // not run it, so leave that tab alone.
                if session.state() == SessionState::Connected && session.screen().alternate_screen()
                {
                    skipped.push(host.name);
                    continue;
                }
                if session.state() == SessionState::Disconnected
                    && let Err(error) = session.reconnect()
                {
                    self.message = Some(error);
                    continue;
                }
                session.run_command(&command);
                self.active_session = Some(session.id);
            } else {
                self.open_host_command(host, &command);
            }
        }
        if !skipped.is_empty() {
            self.message = Some(format!(
                "Not sent to {}: a full-screen program is running in that tab.",
                skipped.join(", ")
            ));
        }
    }

    fn poll(&mut self) {
        #[cfg(target_os = "macos")]
        while let Some(action) = self.menu_bar.next_action() {
            self.handle_menu_bar_action(action);
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(notice) = self.updater.poll() {
                match notice {
                    UpdateNotice::Current => {
                        self.message = Some("ShellHopper is up to date.".into());
                    }
                    UpdateNotice::Ready => self.show_update_confirmation = true,
                    UpdateNotice::Failed(error) => self.message = Some(error),
                }
            }
            self.updater.check_if_due();
        }

        // One budget for all tabs keeps a frame bounded however many are noisy;
        // rotating the starting tab stops the first ones from starving the rest.
        let mut budget = FRAME_CHUNK_BUDGET;
        let mut more = false;
        let count = self.sessions.len();
        for offset in 0..count {
            more |= self.sessions[(self.poll_start + offset) % count].poll(&mut budget);
        }
        self.poll_start = self.poll_start.wrapping_add(1);
        if more {
            self.ctx.request_repaint();
        }

        let finished = self
            .import
            .as_ref()
            .map(|(kind, receiver)| (*kind, receiver.try_recv()));
        match finished {
            Some((kind, Ok(hosts))) => {
                self.import = None;
                let count = self.config.apply_import(kind, hosts);
                self.save_config();
                if kind == ImportKind::Reload {
                    self.message = Some(format!("Imported {count} new SSH host(s)."));
                }
            }
            Some((_, Err(TryRecvError::Disconnected))) => self.import = None,
            _ => {}
        }

        let alias_finished = self
            .alias_check
            .as_ref()
            .map(|(name, receiver)| (name.clone(), receiver.try_recv()));
        match alias_finished {
            Some((name, Ok(Ok(resolved)))) => {
                self.alias_check = None;
                match self.config.use_ssh_alias(&name, &resolved) {
                    Ok(host) => {
                        for session in &mut self.sessions {
                            session.replace_equivalent_host(&name, &host);
                        }
                        self.save_config();
                        self.message = Some(format!(
                            "{name} now uses its SSH config alias. Live sessions were left open; the alias applies on their next reconnect."
                        ));
                    }
                    Err(error) => self.message = Some(error),
                }
            }
            Some((_, Ok(Err(error)))) => {
                self.alias_check = None;
                self.message = Some(error);
            }
            Some((_, Err(TryRecvError::Disconnected))) => {
                self.alias_check = None;
                self.message = Some("The SSH alias check stopped unexpectedly.".into());
            }
            _ => {}
        }

        let upload_finished = self
            .upload
            .as_ref()
            .map(|upload| (upload.host.clone(), upload.receiver.try_recv()));
        match upload_finished {
            Some((host, Ok(Ok(count)))) => {
                self.upload = None;
                self.message = Some(format!(
                    "Uploaded {count} file{} to {host}'s home directory.",
                    if count == 1 { "" } else { "s" }
                ));
            }
            Some((_, Ok(Err(error)))) => {
                self.upload = None;
                self.message = Some(error);
            }
            Some((_, Err(TryRecvError::Disconnected))) => {
                self.upload = None;
                self.message = Some("The file upload stopped unexpectedly.".into());
            }
            _ => {}
        }
    }

    #[cfg(target_os = "macos")]
    fn handle_menu_bar_action(&mut self, action: MenuBarAction) {
        match action {
            MenuBarAction::Show => {}
            MenuBarAction::NewLocal => self.open_local(),
            MenuBarAction::CheckForUpdates => self.updater.check(true),
            MenuBarAction::ToggleWordWrap => {
                self.config.no_word_wrap = !self.config.no_word_wrap;
                self.save_config();
            }
            MenuBarAction::Quit => {
                self.ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            MenuBarAction::OpenHost(name) => {
                let existing = self
                    .sessions
                    .iter()
                    .rposition(|session| {
                        session.is_host(&name) && session.state() != SessionState::Disconnected
                    })
                    .or_else(|| {
                        self.sessions
                            .iter()
                            .rposition(|session| session.is_host(&name))
                    });
                if let Some(index) = existing {
                    if self.sessions[index].state() == SessionState::Disconnected
                        && let Err(error) = self.sessions[index].reconnect()
                    {
                        self.message = Some(error);
                    }
                    let id = self.sessions[index].id;
                    self.active_session = Some(id);
                    self.terminal_focus = Some(id);
                } else if let Some(host) = self
                    .config
                    .hosts
                    .iter()
                    .find(|host| host.name == name)
                    .cloned()
                {
                    self.open_host(host, true);
                }
            }
        }
        MenuBar::activate_app();
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Visible(true));
        self.ctx
            .send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        self.ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn sidebar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("hosts")
            .resizable(true)
            .default_size(280.0)
            .min_size(230.0)
            .max_size(420.0)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(Margin {
                left: 10,
                right: 10,
                top: 0,
                bottom: 12,
            }))
            .show(root, |ui| {
                // The strip beside the traffic lights stays empty and moves the window.
                let (strip, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), TITLEBAR_HEIGHT - 8.0),
                    Sense::hover(),
                );
                theme::window_drag_area(ui, strip, "titlebar");

                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("Hosts ({})", self.config.hosts.len()))
                            .size(15.0)
                            .strong(),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if ui
                            .add(theme::ghost_button("+ Header"))
                            .on_hover_text("Add a sidebar header")
                            .clicked()
                        {
                            self.new_group = Some(String::new());
                        }
                        if ui
                            .add(theme::ghost_button("+ Host"))
                            .on_hover_text("Add an SSH host")
                            .clicked()
                        {
                            self.editor = Some(HostEditor {
                                host: SshHost {
                                    port: 22,
                                    group: self.move_group.clone(),
                                    ..Default::default()
                                },
                                ..Default::default()
                            });
                        }
                    });
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search hosts")
                        .margin(Margin::symmetric(8, 6))
                        .desired_width(f32::INFINITY),
                );

                egui::Panel::bottom("host_actions")
                    .resizable(false)
                    .frame(egui::Frame::new().inner_margin(Margin {
                        top: 12,
                        ..Margin::ZERO
                    }))
                    .show(ui, |ui| self.host_actions(ui));
                egui::CentralPanel::default()
                    .frame(egui::Frame::new().inner_margin(Margin::symmetric(0, 6)))
                    .show(ui, |ui| self.host_list(ui));
            });
    }

    fn host_list(&mut self, ui: &mut egui::Ui) {
        let query = self.search.to_lowercase();
        let mut grouped: BTreeMap<String, Vec<SshHost>> = BTreeMap::new();
        for group in &self.config.groups {
            if query.is_empty() || group.to_lowercase().contains(&query) {
                grouped.entry(group.clone()).or_default();
            }
        }
        for host in self.config.hosts.iter().filter(|host| {
            query.is_empty()
                || host.name.to_lowercase().contains(&query)
                || host.address.to_lowercase().contains(&query)
                || host.group.to_lowercase().contains(&query)
        }) {
            grouped
                .entry(host.group.clone())
                .or_default()
                .push(host.clone());
        }
        if grouped.is_empty() {
            theme::empty_hint(ui, "No hosts match your search");
            return;
        }

        let mut connect = None;
        let mut connect_selected = false;
        let mut edit = None;
        let mut move_hosts = None;
        let mut reorder_hosts = None;
        let mut toggle_separator = None;
        let mut alias_action = None;
        let mut reorder_group = None;
        let groups = self.config.groups.clone();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for group in groups {
                    let Some(hosts) = grouped.remove(&group) else {
                        continue;
                    };
                    let collapsed = self.config.collapsed_groups.contains(&group);
                    let header = theme::host_group(ui, &group, hosts.len(), !collapsed, |ui| {
                        for (host_index, host) in hosts.into_iter().enumerate() {
                            let has_separator = self.config.host_separators.contains(&host.name);
                            if has_separator && host_index > 0 {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 9.0),
                                    Sense::hover(),
                                );
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(rect.left() + 12.0, rect.center().y),
                                        egui::pos2(rect.right() - 12.0, rect.center().y),
                                    ],
                                    Stroke::new(1.0, theme::FAINT),
                                );
                            }
                            let selected = self.selected_hosts.contains(&host.name);
                            let payload = if selected {
                                self.selected_hosts.iter().cloned().collect()
                            } else {
                                vec![host.name.clone()]
                            };
                            let row = theme::host_row(
                                ui,
                                &host.name,
                                &host.address,
                                &host.target(),
                                selected,
                            );
                            row.response.dnd_set_drag_payload(payload);
                            if let Some(dragged) = row.response.dnd_hover_payload::<Vec<String>>()
                                && !dragged.contains(&host.name)
                            {
                                let after = ui.input(|input| {
                                    input.pointer.hover_pos().is_some_and(|position| {
                                        position.y >= row.response.rect.center().y
                                    })
                                });
                                let y = if after {
                                    row.response.rect.bottom()
                                } else {
                                    row.response.rect.top()
                                };
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(row.response.rect.left(), y),
                                        egui::pos2(row.response.rect.right(), y),
                                    ],
                                    Stroke::new(2.0, theme::ACCENT),
                                );
                                if let Some(dragged) =
                                    row.response.dnd_release_payload::<Vec<String>>()
                                {
                                    reorder_hosts = Some((
                                        dragged.as_ref().clone(),
                                        host.name.clone(),
                                        group.clone(),
                                        after,
                                    ));
                                }
                            }
                            let mut action = row.action;
                            row.response.context_menu(|ui| {
                                ui.set_min_width(190.0);
                                if ui.button("Open in new tab").clicked() {
                                    action = Some(HostRowAction::Connect);
                                    ui.close();
                                }
                                if ui
                                    .add_enabled(
                                        !self.selected_hosts.is_empty(),
                                        egui::Button::new("Open selected in tabs"),
                                    )
                                    .clicked()
                                {
                                    action = Some(HostRowAction::ConnectSelected);
                                    ui.close();
                                }
                                ui.separator();
                                if ui.button("Edit host").clicked() {
                                    action = Some(HostRowAction::Edit);
                                    ui.close();
                                }
                                if host.alias.is_empty() {
                                    if ui
                                        .add_enabled(
                                            self.alias_check.is_none(),
                                            egui::Button::new("Use SSH config alias"),
                                        )
                                        .on_hover_text(format!(
                                            "Verify and connect through the Host {} block in ~/.ssh/config",
                                            host.name
                                        ))
                                        .clicked()
                                    {
                                        alias_action = Some((host.name.clone(), true));
                                        ui.close();
                                    }
                                } else if ui.button("Use saved connection settings").clicked() {
                                    alias_action = Some((host.name.clone(), false));
                                    ui.close();
                                }
                                if has_separator || host_index > 0 {
                                    ui.separator();
                                    if has_separator {
                                        if ui.button("Remove separator above").clicked() {
                                            toggle_separator = Some((host.name.clone(), false));
                                            ui.close();
                                        }
                                    } else if ui.button("Add separator above").clicked() {
                                        toggle_separator = Some((host.name.clone(), true));
                                        ui.close();
                                    }
                                }
                            });
                            match action {
                                Some(HostRowAction::Toggle) if selected => {
                                    self.selected_hosts.remove(&host.name);
                                }
                                Some(HostRowAction::Toggle) => {
                                    self.selected_hosts.insert(host.name.clone());
                                }
                                Some(HostRowAction::Connect) => connect = Some(host),
                                Some(HostRowAction::ConnectSelected) => connect_selected = true,
                                Some(HostRowAction::Edit) => edit = Some(host),
                                None => {}
                            }
                        }
                    });
                    if header.clicked() {
                        if collapsed {
                            self.config.collapsed_groups.remove(&group);
                        } else {
                            self.config.collapsed_groups.insert(group.clone());
                        }
                        self.save_config();
                    }
                    header.dnd_set_drag_payload(DraggedGroup(group.clone()));
                    let hovering_hosts = header.dnd_hover_payload::<Vec<String>>().is_some();
                    if hovering_hosts {
                        ui.painter().rect_stroke(
                            header.rect,
                            6,
                            Stroke::new(1.5, theme::ACCENT),
                            egui::StrokeKind::Inside,
                        );
                    }
                    if hovering_hosts
                        && let Some(hosts) = header.dnd_release_payload::<Vec<String>>()
                    {
                        move_hosts = Some((hosts.as_ref().clone(), group.clone()));
                    }
                    let hovering_group = header.dnd_hover_payload::<DraggedGroup>().is_some();
                    if hovering_group {
                        let after = ui.input(|input| {
                            input
                                .pointer
                                .hover_pos()
                                .is_some_and(|position| position.y >= header.rect.center().y)
                        });
                        let y = if after {
                            header.rect.bottom()
                        } else {
                            header.rect.top()
                        };
                        ui.painter().line_segment(
                            [
                                egui::pos2(header.rect.left(), y),
                                egui::pos2(header.rect.right(), y),
                            ],
                            Stroke::new(2.0, theme::ACCENT),
                        );
                    }
                    // A mismatched typed release consumes egui's payload, so check its type first.
                    if hovering_group
                        && let Some(dragged) = header.dnd_release_payload::<DraggedGroup>()
                    {
                        let after = ui.input(|input| {
                            input
                                .pointer
                                .latest_pos()
                                .is_some_and(|position| position.y >= header.rect.center().y)
                        });
                        reorder_group = Some((dragged.0.clone(), group.clone(), after));
                    }
                    ui.add_space(6.0);
                }
            });
        if let Some((group, target, after)) = reorder_group
            && self.config.reorder_group(&group, &target, after)
        {
            self.save_config();
        }
        if let Some((hosts, target, group, after)) = reorder_hosts
            && self
                .config
                .move_hosts(&hosts, &group, Some((&target, after)))
        {
            self.save_config();
        }
        if let Some((hosts, group)) = move_hosts {
            if self.config.move_hosts(&hosts, &group, None) {
                self.save_config();
            }
        }
        if let Some((host, add)) = toggle_separator {
            if add {
                self.config.host_separators.insert(host);
            } else {
                self.config.host_separators.remove(&host);
            }
            self.save_config();
        }
        if let Some((host, use_alias)) = alias_action {
            if use_alias {
                self.start_alias_check(host);
            } else {
                self.stop_using_alias(&host);
            }
        }
        if let Some(host) = connect {
            self.open_host(host, true);
        }
        if connect_selected {
            self.connect_selected();
        }
        if let Some(host) = edit {
            self.editor = Some(HostEditor {
                original_name: Some(host.name.clone()),
                host,
                confirm_delete: false,
            });
        }
    }

    fn host_actions(&mut self, ui: &mut egui::Ui) {
        let any_selected = !self.selected_hosts.is_empty();
        let width = ui.available_width();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} selected", self.selected_hosts.len()))
                    .size(12.0)
                    .color(MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add(theme::ghost_button("Reload config"))
                    .on_hover_text("Import new hosts from ~/.ssh/config")
                    .clicked()
                {
                    self.start_import(ImportKind::Reload);
                }
            });
        });
        if ui
            .add_enabled(
                any_selected,
                theme::primary_button("Connect selected", width),
            )
            .on_hover_text("Open one SSH terminal tab per selected host")
            .clicked()
        {
            self.connect_selected();
        }
        ui.add(
            egui::TextEdit::multiline(&mut self.command)
                .hint_text("Command or multiline script for selected hosts")
                .code_editor()
                .desired_rows(6)
                .margin(Margin::symmetric(8, 6))
                .desired_width(f32::INFINITY),
        );
        if ui
            .add_enabled(
                any_selected && !self.command.trim().is_empty(),
                theme::secondary_button("Run on selected hosts", width),
            )
            .on_hover_text("Run in existing server tabs and open tabs for missing hosts")
            .clicked()
        {
            self.run_selected();
        }
    }

    fn tab_bar(&mut self, root: &mut egui::Ui) {
        let tab_ids = self.tab_ids();
        let (update_version, update_activity) = {
            #[cfg(target_os = "macos")]
            {
                (
                    self.updater.ready_version().map(str::to_owned),
                    self.updater.activity_label(),
                )
            }
            #[cfg(not(target_os = "macos"))]
            {
                (None::<String>, None::<String>)
            }
        };
        egui::Panel::top("tabs")
            .exact_size(TITLEBAR_HEIGHT)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::symmetric(8, 0)),
            )
            .show(root, |ui| {
                let mut activate = None;
                let mut close = None;
                let mut context_action = None;
                let mut rename = None;
                let mut reorder = None;
                let mut new_tab = false;
                let mut update = false;
                let mut controls_right = ui.max_rect().left();
                let mut update_left = None;
                ui.horizontal_centered(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let update_width = if update_version.is_some() {
                        132.0
                    } else if update_activity.is_some() {
                        148.0
                    } else {
                        0.0
                    };
                    egui::ScrollArea::horizontal()
                        .id_salt("terminal_tabs")
                        .max_width((ui.available_width() - 34.0 - update_width).max(40.0))
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .show(ui, |ui| {
                            ui.horizontal_centered(|ui| {
                                for (index, id) in tab_ids.iter().copied().enumerate() {
                                    let Some(session) =
                                        self.sessions.iter().find(|session| session.id == id)
                                    else {
                                        continue;
                                    };
                                    let selected = self
                                        .active_session
                                        .is_some_and(|active| self.tab_root(active) == id);
                                    let (status_color, status_label) = tab_status(session);
                                    let tab = ui
                                        .push_id(("terminal-tab", session.id), |ui| {
                                            theme::tab(
                                                ui,
                                                &session.title,
                                                selected,
                                                true,
                                                status_color,
                                            )
                                        })
                                        .inner;
                                    tab.response
                                        .clone()
                                        .on_hover_cursor(egui::CursorIcon::Grab)
                                        .on_hover_text(status_label);
                                    if tab.response.drag_started_by(egui::PointerButton::Primary) {
                                        self.dragged_tab = Some(session.id);
                                    }
                                    if let Some(dragged) = self.dragged_tab
                                        && dragged != session.id
                                        && ui.input(|input| input.pointer.primary_down())
                                        && ui.input(|input| {
                                            input.pointer.hover_pos().is_some_and(|position| {
                                                tab.response.rect.contains(position)
                                            })
                                        })
                                    {
                                        let after = ui.input(|input| {
                                            input.pointer.hover_pos().is_some_and(|position| {
                                                position.x >= tab.response.rect.center().x
                                            })
                                        });
                                        let x = if after {
                                            tab.response.rect.right()
                                        } else {
                                            tab.response.rect.left()
                                        };
                                        ui.painter().line_segment(
                                            [
                                                egui::pos2(x, tab.response.rect.top()),
                                                egui::pos2(x, tab.response.rect.bottom()),
                                            ],
                                            Stroke::new(2.0, theme::ACCENT),
                                        );
                                        reorder = Some((dragged, session.id, after));
                                    }
                                    tab.response.context_menu(|ui| {
                                        if ui.button("Rename tab…").clicked() {
                                            rename = Some(TabRename {
                                                id: session.id,
                                                title: session.title.clone(),
                                                focus: true,
                                            });
                                            ui.close();
                                        }
                                        ui.separator();
                                        if session.host_name().is_some()
                                            && session.state() == SessionState::Disconnected
                                            && ui.button("Reconnect SSH").clicked()
                                        {
                                            context_action = Some((
                                                index,
                                                session.id,
                                                TabContextAction::Reconnect,
                                            ));
                                            ui.close();
                                        }
                                        if ui.button("Split right    ⌘D").clicked() {
                                            context_action = Some((
                                                index,
                                                session.id,
                                                TabContextAction::SplitRight,
                                            ));
                                            ui.close();
                                        }
                                        if ui.button("Split down    ⇧⌘D").clicked() {
                                            context_action = Some((
                                                index,
                                                session.id,
                                                TabContextAction::SplitDown,
                                            ));
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui.button("Close tab").clicked() {
                                            context_action =
                                                Some((index, session.id, TabContextAction::Close));
                                            ui.close();
                                        }
                                        ui.separator();
                                        if ui
                                            .add_enabled(
                                                index > 0,
                                                egui::Button::new("Close tabs to the left"),
                                            )
                                            .clicked()
                                        {
                                            context_action = Some((
                                                index,
                                                session.id,
                                                TabContextAction::CloseLeft,
                                            ));
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                index + 1 < tab_ids.len(),
                                                egui::Button::new("Close tabs to the right"),
                                            )
                                            .clicked()
                                        {
                                            context_action = Some((
                                                index,
                                                session.id,
                                                TabContextAction::CloseRight,
                                            ));
                                            ui.close();
                                        }
                                        if ui
                                            .add_enabled(
                                                tab_ids.len() > 1,
                                                egui::Button::new("Keep only this tab"),
                                            )
                                            .clicked()
                                        {
                                            context_action = Some((
                                                index,
                                                session.id,
                                                TabContextAction::KeepOnly,
                                            ));
                                            ui.close();
                                        }
                                    });
                                    if tab.close {
                                        close = Some(session.id);
                                    } else if tab.clicked {
                                        activate = Some(session.id);
                                    }
                                }
                            });
                        });
                    let new_tab_response =
                        theme::new_tab_button(ui).on_hover_text("New local terminal");
                    controls_right = new_tab_response.rect.right();
                    new_tab = new_tab_response.clicked();
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if let Some(version) = &update_version {
                            let label = format!("Update to v{version}");
                            let response = ui
                                .add(theme::secondary_button(&label, 128.0))
                                .on_hover_text(
                                    "Restart ShellHopper and install the verified update",
                                );
                            update_left = Some(response.rect.left());
                            update = response.clicked();
                        } else if let Some(label) = update_activity {
                            let response = ui.label(RichText::new(label).size(11.5).color(MUTED));
                            update_left = Some(response.rect.left());
                        }
                    });
                });

                let drag_left = controls_right + 4.0;
                let drag_right = update_left.unwrap_or(ui.max_rect().right()) - 4.0;
                if drag_left < drag_right {
                    theme::window_drag_area(
                        ui,
                        egui::Rect::from_min_max(
                            egui::pos2(drag_left, ui.max_rect().top()),
                            egui::pos2(drag_right, ui.max_rect().bottom()),
                        ),
                        "titlebar",
                    );
                }
                if !ui.input(|input| input.pointer.primary_down()) {
                    self.dragged_tab = None;
                }

                #[cfg(target_os = "macos")]
                if update {
                    self.updater.prepare_to_install();
                }

                if let Some(id) = activate {
                    self.active_session = Some(id);
                    self.terminal_focus = Some(id);
                }
                if let Some(id) = close {
                    self.close_session(id);
                }
                if let Some((dragged, target, after)) = reorder {
                    reorder_session(&mut self.sessions, dragged, target, after);
                }
                if let Some((index, id, action)) = context_action {
                    match action {
                        TabContextAction::Reconnect => {
                            if let Some(session) =
                                self.sessions.iter_mut().find(|session| session.id == id)
                            {
                                if let Err(error) = session.reconnect() {
                                    self.message = Some(error);
                                } else {
                                    self.active_session = Some(id);
                                }
                            }
                        }
                        TabContextAction::SplitRight => {
                            self.active_session = Some(id);
                            self.split_active(SavedSplitDirection::Right);
                        }
                        TabContextAction::SplitDown => {
                            self.active_session = Some(id);
                            self.split_active(SavedSplitDirection::Down);
                        }
                        TabContextAction::Close => self.close_session(id),
                        TabContextAction::CloseLeft => {
                            let ids = tab_ids[..index].to_vec();
                            for id in ids {
                                self.close_session(id);
                            }
                        }
                        TabContextAction::CloseRight => {
                            let ids: Vec<_> = tab_ids[index + 1..].iter().rev().copied().collect();
                            for id in ids {
                                self.close_session(id);
                            }
                        }
                        TabContextAction::KeepOnly => {
                            let ids: Vec<_> = tab_ids
                                .iter()
                                .filter(|root| **root != id)
                                .copied()
                                .collect();
                            for id in ids {
                                self.close_session(id);
                            }
                        }
                    }
                    if !self
                        .sessions
                        .iter()
                        .any(|session| Some(session.id) == self.active_session)
                    {
                        self.active_session = self
                            .sessions
                            .iter()
                            .find(|session| session.id == id)
                            .or_else(|| self.sessions.last())
                            .map(|session| session.id);
                    }
                }
                if let Some(rename) = rename {
                    self.rename_tab = Some(rename);
                }
                if new_tab {
                    self.open_local();
                }
            });
    }

    fn terminal(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if ctx.input(|input| !input.raw.hovered_files.is_empty()) {
            self.file_drop_target = None;
        }
        let Some(id) = self.active_session else {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.4);
                ui.label(RichText::new("No open terminals").color(MUTED));
                ui.add_space(4.0);
                if ui
                    .add(theme::primary_button("Open a local terminal", 0.0))
                    .clicked()
                {
                    self.open_local();
                }
            });
            return;
        };
        let request_focus = self.terminal_focus == Some(id);
        if request_focus {
            self.terminal_focus = None;
        }

        let mut search_direction = None;
        if self.terminal_search_open {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Find").color(MUTED));
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.terminal_search)
                        .hint_text("Search terminal history")
                        .desired_width(260.0)
                        .margin(Margin::symmetric(8, 5)),
                );
                if self.terminal_search_focus {
                    response.request_focus();
                    self.terminal_search_focus = false;
                }
                if response.changed() {
                    self.terminal_search_status = None;
                }

                if response.has_focus()
                    && ui.input_mut(|input| input.consume_key(egui::Modifiers::SHIFT, Key::Enter))
                {
                    search_direction = Some(TerminalSearchDirection::Newer);
                } else if response.has_focus()
                    && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter))
                {
                    search_direction = Some(TerminalSearchDirection::Older);
                }
                if ui
                    .add(theme::ghost_button("↑"))
                    .on_hover_text("Previous match (Shift+Enter)")
                    .clicked()
                {
                    search_direction = Some(TerminalSearchDirection::Newer);
                }
                if ui
                    .add(theme::ghost_button("↓"))
                    .on_hover_text("Next match (Enter)")
                    .clicked()
                {
                    search_direction = Some(TerminalSearchDirection::Older);
                }
                if let Some(status) = &self.terminal_search_status {
                    ui.label(RichText::new(status).size(12.0).color(MUTED));
                }
                if ui
                    .add(theme::ghost_button("Close"))
                    .on_hover_text("Close search (Escape)")
                    .clicked()
                    || (response.has_focus()
                        && ui.input_mut(|input| {
                            input.consume_key(egui::Modifiers::NONE, Key::Escape)
                        }))
                {
                    self.terminal_search_open = false;
                    self.terminal_search_status = None;
                }
            });
            ui.add_space(4.0);
        }

        if let Some(direction) = search_direction {
            let found = self
                .sessions
                .iter_mut()
                .find(|session| session.id == id)
                .is_some_and(|session| {
                    session.search_history(
                        &self.terminal_search,
                        matches!(direction, TerminalSearchDirection::Older),
                    )
                });
            self.terminal_search_status = if found {
                None
            } else if self.terminal_search.trim().is_empty() {
                Some("Enter text to find".into())
            } else {
                Some(
                    match direction {
                        TerminalSearchDirection::Older => "No older matches",
                        TerminalSearchDirection::Newer => "No newer matches",
                    }
                    .into(),
                )
            };
        }

        let root = self.tab_root(id);
        if let Some(layout) = self.splits.get(&root).cloned() {
            self.split_terminal(ui, ctx, root, &layout, request_focus);
        } else {
            self.terminal_pane(ui, ctx, id, request_focus);
        }
    }

    fn split_terminal(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        root: u64,
        layout: &PaneLayout,
        request_focus: bool,
    ) {
        let rect = ui.available_rect_before_wrap();
        let close = self.pane_layout(ui, ctx, root, layout, request_focus);
        ui.advance_cursor_after_rect(rect);
        if let Some(id) = close {
            self.close_pane_or_tab(id);
        }
    }

    fn pane_layout(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        root: u64,
        layout: &PaneLayout,
        request_focus: bool,
    ) -> Option<u64> {
        let PaneLayout::Split {
            id,
            direction,
            ratio,
            first,
            second,
        } = layout
        else {
            let PaneLayout::Leaf(id) = layout else {
                unreachable!();
            };
            return self
                .split_pane(
                    ui,
                    ctx,
                    *id,
                    request_focus && self.active_session == Some(*id),
                )
                .then_some(*id);
        };

        let rect = ui.available_rect_before_wrap();
        let divider = 6.0;
        let ratio = ratio.clamp(0.2, 0.8);
        let (first_rect, divider_rect, second_rect) = match direction {
            SavedSplitDirection::Right => {
                let available = (rect.width() - divider).max(1.0);
                let first = available * ratio;
                (
                    egui::Rect::from_min_max(
                        rect.min,
                        egui::pos2(rect.left() + first, rect.bottom()),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left() + first, rect.top()),
                        egui::pos2(rect.left() + first + divider, rect.bottom()),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left() + first + divider, rect.top()),
                        rect.max,
                    ),
                )
            }
            SavedSplitDirection::Down => {
                let available = (rect.height() - divider).max(1.0);
                let first = available * ratio;
                (
                    egui::Rect::from_min_max(
                        rect.min,
                        egui::pos2(rect.right(), rect.top() + first),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left(), rect.top() + first),
                        egui::pos2(rect.right(), rect.top() + first + divider),
                    ),
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left(), rect.top() + first + divider),
                        rect.max,
                    ),
                )
            }
        };

        let divider_response = ui.interact(
            divider_rect,
            egui::Id::new(("terminal-divider", root, *id)),
            Sense::drag(),
        );
        if divider_response.dragged() {
            let delta = ctx.input(|input| input.pointer.delta());
            let adjustment = match direction {
                SavedSplitDirection::Right => delta.x / rect.width().max(1.0),
                SavedSplitDirection::Down => delta.y / rect.height().max(1.0),
            };
            if let Some(saved) = self.splits.get_mut(&root) {
                saved.adjust_ratio(*id, adjustment);
            }
            ctx.request_repaint();
        }
        let line = match direction {
            SavedSplitDirection::Right => [divider_rect.center_top(), divider_rect.center_bottom()],
            SavedSplitDirection::Down => [divider_rect.left_center(), divider_rect.right_center()],
        };
        ui.painter()
            .line_segment(line, Stroke::new(1.0, theme::BORDER));

        let mut first_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("split-pane", root, *id, false))
                .max_rect(first_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        let close_first = self.pane_layout(&mut first_ui, ctx, root, first, request_focus);
        let mut second_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt(("split-pane", root, *id, true))
                .max_rect(second_rect)
                .layout(Layout::top_down(Align::Min)),
        );
        let close_second = self.pane_layout(&mut second_ui, ctx, root, second, request_focus);
        ui.advance_cursor_after_rect(rect);
        close_first.or(close_second)
    }

    fn split_pane(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        id: u64,
        request_focus: bool,
    ) -> bool {
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            return false;
        };
        let title = session.title.clone();
        let (status, _) = tab_status(session);
        let active = self.active_session == Some(id);
        let mut close = false;
        ui.horizontal(|ui| {
            let label = ui
                .selectable_label(active, RichText::new(title).size(12.0).color(status))
                .on_hover_text("Click to focus this pane");
            if label.clicked() {
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close = ui
                    .add(theme::ghost_button("×"))
                    .on_hover_text("Close pane")
                    .clicked();
            });
        });
        ui.separator();
        self.terminal_pane(ui, ctx, id, request_focus);
        close
    }

    fn terminal_pane(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        id: u64,
        request_focus: bool,
    ) {
        let search = self
            .terminal_search_open
            .then_some(self.terminal_search.trim())
            .filter(|query| !query.is_empty())
            .map(str::to_owned);
        let close_label = if self.splits.contains_key(&self.tab_root(id)) {
            "Close pane"
        } else {
            "Close tab"
        };
        let active = self.active_session == Some(id);
        let groups = &self.config.groups;
        let hosts = &self.config.hosts;
        let saved_commands = self.config.saved_commands.clone();
        let word_wrap = !self.config.no_word_wrap;
        let mut file_drop_target = self.file_drop_target;
        let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) else {
            return;
        };

        if session.scrollback() > 0 {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} lines back", session.scrollback()))
                        .size(12.0)
                        .color(MUTED),
                );
                if ui
                    .add(theme::secondary_button("Return to live", 104.0))
                    .clicked()
                {
                    session.scroll_to_live();
                }
            });
        }

        let font = FontId::monospace(TERM_FONT_SIZE);
        let cell_width = ctx.fonts_mut(|fonts| fonts.glyph_width(&font, 'M'));
        let row_height = TERM_LINE_HEIGHT;
        let cols = (ui.available_width() / cell_width)
            .floor()
            .clamp(20.0, u16::MAX as f32) as u16;
        let show_command_editor = session.editor_active();
        let pane_height = (ui.clip_rect().bottom() - ui.cursor().top()).max(row_height * 2.0);
        let terminal_height = pane_height;
        let rows = (terminal_height / row_height)
            .floor()
            .clamp(2.0, u16::MAX as f32) as u16;
        session.resize(rows, cols);
        // A multi-line command needs blank rows under the prompt, so scroll the
        // screen up first, the way the shell would if it were drawing the text.
        // With word wrap on, the editor spans the pane like the shell's own echo
        // of the command: the first row starts after the prompt, later rows at
        // the left edge. With it off, or while empty (an empty layout puts its
        // cursor at the left edge), the whole editor starts after the prompt.
        let pane_width = f32::from(cols) * cell_width;
        let editor_wrap_width = if word_wrap {
            pane_width + 0.5
        } else {
            f32::INFINITY
        };
        let mut editor_offset = 0.0;
        let mut editor_indent = 0.0;
        let mut editor_rows = 1;
        if show_command_editor {
            let prompt_width =
                f32::from(command_editor_column(session.screen(), cols)) * cell_width;
            if word_wrap && !session.command_input_mut().is_empty() {
                editor_indent = prompt_width;
            } else {
                editor_offset = prompt_width;
            }
            let job = command_editor_job(
                session.command_input_mut(),
                editor_indent,
                editor_wrap_width,
            );
            let height = ctx.fonts_mut(|fonts| fonts.layout_job(job)).size().y;
            editor_rows = (height / row_height).ceil().clamp(1.0, f32::from(rows)) as u16;
            session.reserve_rows_below_cursor(editor_rows - 1);
        }

        if self
            .terminal_selection
            .is_some_and(|selection| selection.session_id == id && selection.cols != cols)
        {
            self.terminal_selection = None;
        }
        let history_rows = session.history_rows();
        let selection = self
            .terminal_selection
            .filter(|selection| selection.session_id == id);
        let terminal = terminal_layout(
            session.screen(),
            search.as_deref(),
            history_rows,
            selection.as_ref(),
            !show_command_editor,
        );
        let screen = session.screen();
        let application_cursor = screen.application_cursor();
        let bracketed_paste = screen.bracketed_paste();
        let alternate_screen = screen.alternate_screen();
        let mouse_mode = screen.mouse_protocol_mode();
        let sgr_mouse = mouse_mode != vt100::MouseProtocolMode::None
            && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr;
        let shift_held = ctx.input(|input| input.modifiers.shift);
        let terminal_selectable = !sgr_mouse || shift_held;
        let mut activated = false;
        let mut connect_host = None;
        let mut reconnect = false;
        let mut split = None;
        let mut close = false;
        let mut copy_selection = false;
        let mut delete_selection = false;
        let mut paste_from_clipboard = false;
        let mut paste_saved_command = None;
        let mut manage_saved_commands = false;
        let mut open_error = None;
        let mut dropped_paths = Vec::new();
        let mut terminal_response_id = None;
        let mut terminal_response_rect = None;
        let mut focus_editor = false;
        let mut copy_editor = false;
        let mut cut_editor = false;
        let editor_id = egui::Id::new(("command-editor", id));
        let editor_selection = show_command_editor
            .then(|| command_editor_selection(ctx, editor_id))
            .flatten();
        // The screen rows the command editor covers. Clicks there are the
        // editor's, never the start of an output selection.
        let editor_band = show_command_editor.then(|| {
            let row = session.screen().cursor_position().0;
            (row, editor_rows.min(rows.saturating_sub(row)).max(1))
        });
        let mut collapse_editor_selection = false;
        let reconnectable =
            session.host_name().is_some() && session.state() == SessionState::Disconnected;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(terminal_height)
            .scroll_source(egui::scroll_area::ScrollSource::NONE)
            .show(ui, |ui| {
                let response = ui
                    .push_id(("terminal", id), |ui| {
                        ui.add(
                            egui::Label::new(terminal)
                                .selectable(false)
                                .sense(Sense::click_and_drag())
                                .wrap_mode(egui::TextWrapMode::Extend),
                        )
                    })
                    .inner;
                terminal_response_id = Some(response.id);
                terminal_response_rect = Some(response.rect);
                let input_state = TerminalInputState {
                    rect: response.rect,
                    cell_width,
                    row_height,
                    rows,
                    cols,
                    application_cursor,
                    bracketed_paste,
                    alternate_screen,
                    mouse_mode,
                    sgr_mouse,
                };
                let editor_band_rect = editor_band.map(|(row, count)| {
                    egui::Rect::from_min_size(
                        egui::pos2(
                            response.rect.left(),
                            response.rect.top() + f32::from(row) * row_height,
                        ),
                        egui::vec2(response.rect.width(), f32::from(count) * row_height),
                    )
                });
                let selection_start = ctx.input(|input| {
                    input.events.iter().find_map(|event| match event {
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers,
                        } if terminal_selectable
                            && !modifiers.command
                            && response.rect.contains(*pos)
                            && !editor_band_rect.is_some_and(|rect| rect.contains(*pos)) =>
                        {
                            Some((*pos, modifiers.shift))
                        }
                        _ => None,
                    })
                });
                // A press on a menu or window that floats over the terminal is
                // not a press on the terminal, and must not restart the
                // selection the menu is about to copy.
                let selection_start = selection_start.filter(|(position, _)| {
                    ctx.layer_id_at(*position)
                        .is_none_or(|layer| layer == response.layer_id)
                });
                if let Some((position, extend)) = selection_start {
                    collapse_editor_selection = true;
                    let point = terminal_selection_point(
                        position,
                        input_state,
                        history_rows,
                        session.scrollback(),
                    );
                    if extend
                        && let Some(selection) = self
                            .terminal_selection
                            .as_mut()
                            .filter(|selection| selection.session_id == id)
                    {
                        selection.head = point;
                        selection.dragging = true;
                    } else {
                        self.terminal_selection = Some(TerminalSelection {
                            session_id: id,
                            anchor: point,
                            head: point,
                            cols,
                            dragging: true,
                        });
                    }
                    ctx.request_repaint();
                }
                if self
                    .terminal_selection
                    .is_some_and(|selection| selection.session_id == id && selection.dragging)
                {
                    let (position, primary_down) = ctx.input(|input| {
                        (input.pointer.interact_pos(), input.pointer.primary_down())
                    });
                    if primary_down {
                        if let Some(position) = position {
                            let lines = if position.y < response.rect.top() {
                                ((response.rect.top() - position.y) / row_height)
                                    .ceil()
                                    .clamp(1.0, 6.0) as isize
                            } else if position.y > response.rect.bottom() {
                                -((position.y - response.rect.bottom()) / row_height)
                                    .ceil()
                                    .clamp(1.0, 6.0) as isize
                            } else {
                                0
                            };
                            if lines != 0 && !alternate_screen {
                                session.scroll_lines(lines);
                            }
                            let history_rows = session.history_rows();
                            let point = terminal_selection_point(
                                position,
                                input_state,
                                history_rows,
                                session.scrollback(),
                            );
                            let mut changed = false;
                            if let Some(selection) = self.terminal_selection.as_mut() {
                                if selection.head != point {
                                    selection.head = point;
                                    changed = true;
                                }
                            }
                            if lines != 0 || changed {
                                ctx.request_repaint();
                            }
                        }
                    } else if let Some(selection) = self.terminal_selection.as_mut() {
                        selection.dragging = false;
                        if !selection.has_text() {
                            self.terminal_selection = None;
                        }
                    }
                }
                if terminal_selectable
                    && (response.double_clicked() || response.triple_clicked())
                    && let Some((col, row)) = response
                        .interact_pointer_pos()
                        .and_then(|position| terminal_cell(position, input_state))
                {
                    let range = if response.triple_clicked() {
                        Some((0, cols))
                    } else {
                        terminal_word_range(session.screen(), row - 1, col - 1)
                    };
                    if let Some((start, end)) = range {
                        let row = history_rows.saturating_sub(session.scrollback())
                            + usize::from(row - 1);
                        self.terminal_selection = Some(TerminalSelection {
                            session_id: id,
                            anchor: TerminalPoint { row, col: start },
                            head: TerminalPoint { row, col: end },
                            cols,
                            dragging: false,
                        });
                        collapse_editor_selection = true;
                    }
                }
                let command_held = ctx.input(|input| input.modifiers.command);
                let hovered_target = command_held
                    .then(|| response.hover_pos())
                    .flatten()
                    .and_then(|position| terminal_cell(position, input_state))
                    .and_then(|(col, row)| {
                        terminal_open_target_at(
                            session.screen(),
                            row - 1,
                            col - 1,
                            !session.is_remote(),
                        )
                    });
                if let Some(target) = hovered_target {
                    ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                    response
                        .clone()
                        .on_hover_text_at_pointer(target.hover_text());
                    if response.clicked_by(egui::PointerButton::Primary)
                        && let Err(error) = open_terminal_target(ctx, target)
                    {
                        open_error = Some(error);
                    }
                }
                let pointer_over = ctx.input(|input| {
                    input
                        .pointer
                        .hover_pos()
                        .is_some_and(|position| response.rect.contains(position))
                });
                if pointer_over && ctx.input(|input| !input.raw.hovered_files.is_empty()) {
                    file_drop_target = Some(id);
                    let label = if !session.is_remote() {
                        "Drop onto a connected SSH pane to upload"
                    } else if session.state() != SessionState::Connected {
                        "Reconnect this SSH pane before uploading"
                    } else {
                        "Drop to upload to the remote home directory"
                    };
                    ui.painter().rect_stroke(
                        response.rect,
                        6,
                        Stroke::new(2.0, theme::ACCENT),
                        egui::StrokeKind::Inside,
                    );
                    let overlay = egui::Rect::from_center_size(
                        response.rect.center(),
                        egui::vec2(310.0, 38.0),
                    );
                    ui.painter().rect_filled(overlay, 8, SIDEBAR);
                    ui.painter().text(
                        overlay.center(),
                        Align2::CENTER_CENTER,
                        label,
                        FontId::proportional(13.0),
                        Color32::WHITE,
                    );
                }
                if file_drop_target == Some(id) || (file_drop_target.is_none() && active) {
                    dropped_paths = ctx.input(|input| {
                        input
                            .raw
                            .dropped_files
                            .iter()
                            .map(|file| file.path().to_path_buf())
                            .collect()
                    });
                    if !dropped_paths.is_empty() {
                        file_drop_target = None;
                    }
                }
                // The editor sits on top of the terminal, so a right-click on
                // it opens the same menu.
                let editor_secondary_click = editor_band_rect.is_some_and(|rect| {
                    ctx.input(|input| {
                        input.pointer.secondary_clicked()
                            && input
                                .pointer
                                .interact_pos()
                                .is_some_and(|position| rect.contains(position))
                    })
                });
                let menu_state = if response.secondary_clicked() || editor_secondary_click {
                    Some(egui::SetOpenCommand::Bool(true))
                } else if response.clicked() {
                    Some(egui::SetOpenCommand::Bool(false))
                } else {
                    None
                };
                egui::Popup::menu(&response)
                    .open_memory(menu_state)
                    .at_pointer_fixed()
                    .show(|ui| {
                        let can_copy = editor_selection.is_some()
                            || self.terminal_selection.is_some_and(|selection| {
                                selection.session_id == id && selection.has_text()
                            });
                        if ui.button("Select All Output     ⌘A").clicked() {
                            let (cursor_row, cursor_col) = session.screen().cursor_position();
                            self.terminal_selection = Some(TerminalSelection {
                                session_id: id,
                                anchor: TerminalPoint { row: 0, col: 0 },
                                head: TerminalPoint {
                                    row: history_rows + usize::from(cursor_row),
                                    col: cursor_col,
                                },
                                cols,
                                dragging: false,
                            });
                            ui.close();
                        }
                        if show_command_editor
                            && ui
                                .add_enabled(
                                    editor_selection.is_some(),
                                    egui::Button::new("Cut      ⌘X"),
                                )
                                .clicked()
                        {
                            cut_editor = true;
                            ui.close();
                        }
                        if ui
                            .add_enabled(can_copy, egui::Button::new("Copy     ⌘C"))
                            .clicked()
                        {
                            if editor_selection.is_some() {
                                copy_editor = true;
                            } else {
                                copy_selection = true;
                            }
                            ui.close();
                        }
                        if ui.button("Paste    ⌘V").clicked() {
                            paste_from_clipboard = true;
                            ui.close();
                        }
                        ui.separator();
                        ui.menu_button("Connect to host", |ui| {
                            for group in groups {
                                ui.menu_button(group, |ui| {
                                    for host in hosts.iter().filter(|host| host.group == *group) {
                                        if ui.button(&host.name).clicked() {
                                            connect_host = Some(host.name.clone());
                                            ui.close();
                                        }
                                    }
                                });
                            }
                        });
                        ui.menu_button("Saved commands", |ui| {
                            if saved_commands.is_empty() {
                                ui.add_enabled(false, egui::Button::new("No saved commands"));
                            } else {
                                for saved in &saved_commands {
                                    let safe = !saved.command.contains(['\r', '\n']);
                                    let connected = session.state() == SessionState::Connected;
                                    if ui
                                        .add_enabled(
                                            safe && connected && !alternate_screen,
                                            egui::Button::new(&saved.name),
                                        )
                                        .on_disabled_hover_text(if !safe {
                                            "Saved commands must be one line"
                                        } else if !connected {
                                            "Connect this terminal before pasting a command"
                                        } else {
                                            "Leave the full-screen program before pasting a command"
                                        })
                                        .clicked()
                                    {
                                        paste_saved_command = Some(saved.command.clone());
                                        ui.close();
                                    }
                                }
                            }
                            ui.separator();
                            if ui.button("Manage saved commands…").clicked() {
                                manage_saved_commands = true;
                                ui.close();
                            }
                        });
                        ui.separator();
                        if reconnectable && ui.button("Reconnect SSH").clicked() {
                            reconnect = true;
                            ui.close();
                        }
                        if ui.button("Split right    ⌘D").clicked() {
                            split = Some(SavedSplitDirection::Right);
                            ui.close();
                        }
                        if ui.button("Split down    ⇧⌘D").clicked() {
                            split = Some(SavedSplitDirection::Down);
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(close_label).clicked() {
                            close = true;
                            ui.close();
                        }
                    });
                if request_focus && !show_command_editor {
                    response.request_focus();
                }
                let pointer_pressed =
                    response.hovered() && ctx.input(|input| input.pointer.any_pressed());
                if pointer_pressed
                    || response.clicked()
                    || response.drag_started_by(egui::PointerButton::Primary)
                {
                    if show_command_editor {
                        focus_editor = true;
                    } else {
                        response.request_focus();
                    }
                    activated = true;
                }
                if show_command_editor && response.has_focus() {
                    focus_editor = true;
                }
                if response.hovered() && !alternate_screen && (!sgr_mouse || shift_held) {
                    let scroll = ctx.input(|input| input.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        session.scroll_by_points(scroll, row_height);
                        ctx.request_repaint();
                    }
                }
                if response.has_focus() && !show_command_editor {
                    if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::A)) {
                        let (cursor_row, cursor_col) = session.screen().cursor_position();
                        self.terminal_selection = Some(TerminalSelection {
                            session_id: id,
                            anchor: TerminalPoint { row: 0, col: 0 },
                            head: TerminalPoint {
                                row: history_rows + usize::from(cursor_row),
                                col: cursor_col,
                            },
                            cols,
                            dragging: false,
                        });
                    }
                    ui.memory_mut(|memory| {
                        memory.set_focus_lock_filter(
                            response.id,
                            egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            },
                        );
                    });
                    if ctx.input(|input| {
                        input
                            .events
                            .iter()
                            .any(|event| matches!(event, egui::Event::Copy))
                    }) {
                        copy_selection = true;
                    }
                    delete_selection =
                        handle_terminal_input(ctx, session, input_state, selection, history_rows);
                }
            });
        if (copy_editor || cut_editor)
            && let Some(range) = &editor_selection
        {
            let input = session.command_input_mut();
            let byte = |index: usize| {
                input
                    .char_indices()
                    .nth(index)
                    .map_or(input.len(), |(byte, _)| byte)
            };
            let bytes = byte(range.start)..byte(range.end);
            ctx.copy_text(input[bytes.clone()].to_owned());
            if cut_editor {
                input.replace_range(bytes, "");
                if let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id) {
                    let start = egui::text::CCursor::new(range.start);
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::one(start)));
                    state.store(ctx, editor_id);
                }
            }
            focus_editor = true;
        }
        let mut submit_command = false;
        let mut complete_command = false;
        let mut editor_touched = false;
        if show_command_editor {
            // Focus first, so a paste or key that arrives this frame lands here.
            if request_focus || focus_editor {
                ctx.memory_mut(|memory| memory.request_focus(editor_id));
            }
            // A right-click would otherwise move the editor's cursor and drop
            // the highlight the menu is meant to act on.
            let keep_editor_selection = editor_selection.clone().filter(|_| {
                !cut_editor
                    && ctx.input(|input| {
                        let button = egui::PointerButton::Secondary;
                        input.pointer.button_pressed(button)
                            || input.pointer.button_down(button)
                            || input.pointer.button_released(button)
                    })
            });
            if ctx.memory(|memory| memory.has_focus(editor_id)) {
                // In a multi-line command the arrows move between its lines.
                let single_line = !session.command_input_mut().contains('\n');
                let (submit, older, newer, tab, interrupt, end_of_input, clear) =
                    ctx.input_mut(|input| {
                        (
                            !input.modifiers.shift
                                && input.consume_key(egui::Modifiers::NONE, Key::Enter),
                            single_line && input.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                            single_line && input.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                            input.consume_key(egui::Modifiers::NONE, Key::Tab),
                            input.consume_key(egui::Modifiers::CTRL, Key::C),
                            input.consume_key(egui::Modifiers::CTRL, Key::D),
                            input.consume_key(egui::Modifiers::CTRL, Key::L),
                        )
                    });
                let empty = session.command_input_mut().is_empty();
                if older {
                    session.recall_command(true);
                }
                if newer {
                    session.recall_command(false);
                }
                if interrupt {
                    if empty {
                        session.send(b"\x03");
                    } else {
                        session.take_command_input();
                    }
                }
                if end_of_input && empty {
                    session.send(b"\x04");
                }
                if clear {
                    session.send(b"\x0c");
                }
                complete_command = tab && !empty;
                submit_command = submit;
            }
            // The output selection and the editor selection never coexist, so
            // Copy always has exactly one thing it can mean.
            if collapse_editor_selection
                && let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id)
                && let Some(range) = state.cursor.char_range()
            {
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(range.primary)));
                state.store(ctx, editor_id);
            }
            if session.take_input_cursor_to_end()
                && let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id)
            {
                let end = egui::text::CCursor::new(session.command_input_mut().chars().count());
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                state.store(ctx, editor_id);
            }
            if let Some(terminal_rect) = terminal_response_rect {
                let (cursor_row, _) = session.screen().cursor_position();
                let left = terminal_rect.left() + editor_offset;
                let top = terminal_rect.top() + f32::from(cursor_row) * row_height;
                let visible_rows = editor_rows.min(rows.saturating_sub(cursor_row)).max(1);
                let editor_rect = egui::Rect::from_min_size(
                    egui::pos2(left, top),
                    egui::vec2(
                        (pane_width - editor_offset).max(cell_width),
                        f32::from(visible_rows) * row_height,
                    ),
                );
                let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, _: f32| {
                    let job = command_editor_job(text.as_str(), editor_indent, editor_wrap_width);
                    ui.ctx().fonts_mut(|fonts| fonts.layout_job(job))
                };
                let text_edit = egui::TextEdit::multiline(session.command_input_mut())
                    .id(editor_id)
                    .font(FontId::monospace(TERM_FONT_SIZE))
                    .layouter(&mut layouter)
                    .desired_width(editor_rect.width())
                    .desired_rows(1)
                    .margin(Margin::ZERO)
                    .frame(egui::Frame::NONE)
                    .return_key(egui::KeyboardShortcut::new(
                        egui::Modifiers::SHIFT,
                        Key::Enter,
                    ))
                    .lock_focus(true);
                let editor = if word_wrap {
                    ui.place(editor_rect, text_edit)
                } else {
                    // Unwrapped lines can run past the pane, so they scroll
                    // sideways to follow the cursor.
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
                    egui::ScrollArea::horizontal()
                        .id_salt(("command-editor-scroll", id))
                        .auto_shrink([false, false])
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .show(&mut child, |ui| ui.add(text_edit))
                        .inner
                };
                if (request_focus || focus_editor) && !submit_command {
                    editor.request_focus();
                }
                if let Some(range) = keep_editor_selection
                    && let Some(mut state) = egui::TextEdit::load_state(ctx, editor_id)
                {
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::two(
                            egui::text::CCursor::new(range.start),
                            egui::text::CCursor::new(range.end),
                        )));
                    state.store(ctx, editor_id);
                }
                let editor_selected = egui::TextEdit::load_state(ctx, editor_id)
                    .and_then(|state| state.cursor.char_range())
                    .is_some_and(|range| !range.is_empty());
                if editor.has_focus() {
                    ui.memory_mut(|memory| {
                        memory.set_focus_lock_filter(
                            editor_id,
                            egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            },
                        );
                    });
                    if !editor_selected
                        && ctx.input(|input| {
                            input
                                .events
                                .iter()
                                .any(|event| matches!(event, egui::Event::Copy))
                        })
                    {
                        copy_selection = true;
                    }
                }
                editor_touched = editor_selected
                    || editor.changed()
                    || editor.clicked()
                    || editor.drag_started();
            }
        }
        if editor_touched
            && self
                .terminal_selection
                .is_some_and(|selection| selection.session_id == id)
        {
            self.terminal_selection = None;
        }
        if delete_selection {
            self.terminal_selection = None;
        }
        if copy_selection
            && let Some(selection) = self
                .terminal_selection
                .filter(|selection| selection.session_id == id && selection.has_text())
        {
            let (start, end) = selection.range();
            ctx.copy_text(session.selection_text(start.row, start.col, end.row, end.col));
        }
        if paste_from_clipboard {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            self.active_session = Some(id);
            self.terminal_focus = Some(id);
        }
        if let Some(command) = paste_saved_command {
            paste_command(session, &command, bracketed_paste);
            self.active_session = Some(id);
            self.terminal_focus = Some(id);
        }
        if submit_command || complete_command {
            let command = session.take_command_input();
            let command = if command.trim().is_empty() {
                ""
            } else {
                command.as_str()
            };
            let mut bytes = shell_buffer_sequence(command);
            if submit_command {
                bytes.extend_from_slice(SHELL_ACCEPT_LINE);
            } else {
                bytes.push(b'\t');
            }
            session.hand_off(&bytes);
            if let Some(response_id) = terminal_response_id {
                ctx.memory_mut(|memory| memory.request_focus(response_id));
            }
        }
        self.file_drop_target = file_drop_target;
        if !dropped_paths.is_empty() {
            let host = session.host_name().map(str::to_owned);
            if self.upload.is_some() || self.pending_upload.is_some() {
                self.message = Some("Wait for the current file upload to finish.".into());
            } else if host.is_none() {
                self.message = Some("Drop files onto a connected SSH pane to upload them.".into());
            } else if session.state() != SessionState::Connected {
                self.message = Some("Reconnect this SSH pane before uploading files.".into());
            } else if let Some(path) = dropped_paths.iter().find(|path| !path.is_file()) {
                self.message = Some(format!(
                    "{} is not a regular file. Folder uploads are not supported yet.",
                    path.display()
                ));
            } else {
                self.pending_upload = Some(PendingUpload {
                    session_id: id,
                    host: host.unwrap_or_default(),
                    paths: dropped_paths,
                });
            }
        }
        if let Some(error) = open_error {
            self.message = Some(error);
        }
        if activated {
            self.active_session = Some(id);
        }
        if let Some(host) = connect_host.and_then(|name| {
            self.config
                .hosts
                .iter()
                .find(|host| host.name == name)
                .cloned()
        }) {
            self.connect_pane(id, host);
        }
        if reconnect {
            if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
                if let Err(error) = session.reconnect() {
                    self.message = Some(error);
                } else {
                    self.active_session = Some(id);
                    self.terminal_focus = Some(id);
                }
            }
        }
        if let Some(direction) = split {
            self.active_session = Some(id);
            self.split_active(direction);
        }
        if close {
            self.close_pane_or_tab(id);
        }
        if manage_saved_commands {
            self.saved_commands_editor = Some(saved_commands);
        }
    }

    fn saved_commands_editor(&mut self, root: &mut egui::Ui) {
        let Some(commands) = &mut self.saved_commands_editor else {
            return;
        };
        let mut open = true;
        let mut save = false;
        let mut cancel = false;
        let mut add = false;
        let mut delete = None;
        let mut move_command = None;
        let validation_error = saved_commands_error(commands);

        egui::Window::new("Saved commands")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([680.0, 320.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(root, |ui| {
                ui.set_min_width(620.0);
                ui.label(
                    RichText::new(
                        "Choosing a saved command pastes it into the terminal. Press Enter to run it.",
                    )
                    .color(MUTED),
                );
                ui.add_space(8.0);

                if commands.is_empty() {
                    ui.label("No saved commands yet.");
                } else {
                    let command_count = commands.len();
                    egui::ScrollArea::vertical().max_height(210.0).show(ui, |ui| {
                        for (index, saved) in commands.iter_mut().enumerate() {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut saved.name)
                                        .hint_text("Name")
                                        .desired_width(155.0),
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut saved.command)
                                        .hint_text("Command")
                                        .code_editor()
                                        .desired_width(330.0),
                                );
                                if ui
                                    .add_enabled(index > 0, egui::Button::new("↑"))
                                    .on_hover_text("Move up")
                                    .clicked()
                                {
                                    move_command = Some((index, index - 1));
                                }
                                if ui
                                    .add_enabled(index + 1 < command_count, egui::Button::new("↓"))
                                    .on_hover_text("Move down")
                                    .clicked()
                                {
                                    move_command = Some((index, index + 1));
                                }
                                if ui.button("Delete").clicked() {
                                    delete = Some(index);
                                }
                            });
                        }
                    });
                }

                if let Some(error) = &validation_error {
                    ui.add_space(4.0);
                    ui.label(RichText::new(error).color(RED));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Add command").clicked() {
                        add = true;
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                validation_error.is_none(),
                                theme::primary_button("Save", 72.0),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                        if ui.add(theme::secondary_button("Cancel", 72.0)).clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if let Some((from, to)) = move_command {
            commands.swap(from, to);
        }
        if let Some(index) = delete {
            commands.remove(index);
        }
        if add {
            commands.push(SavedCommand::default());
        }
        if save {
            let mut commands = self.saved_commands_editor.take().unwrap_or_default();
            for saved in &mut commands {
                saved.name = saved.name.trim().to_owned();
                saved.command = saved.command.trim().to_owned();
            }
            self.config.saved_commands = commands;
            self.save_config();
        } else if cancel || !open {
            self.saved_commands_editor = None;
        }
    }

    fn tab_rename_editor(&mut self, root: &mut egui::Ui) {
        let Some(rename) = &mut self.rename_tab else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Rename tab")
            .collapsible(false)
            .resizable(false)
            .fixed_size([332.0, 140.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(root, |ui| {
                ui.set_min_width(300.0);
                let response = ui.add(
                    egui::TextEdit::singleline(&mut rename.title)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::symmetric(8, 6)),
                );
                if rename.focus {
                    response.request_focus();
                    rename.focus = false;
                }
                if response.has_focus()
                    && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Enter))
                    && !rename.title.trim().is_empty()
                {
                    save = true;
                }
                if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                    cancel = true;
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !rename.title.trim().is_empty(),
                                theme::primary_button("Rename", 76.0),
                            )
                            .clicked()
                        {
                            save = true;
                        }
                        if ui.add(theme::secondary_button("Cancel", 72.0)).clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if cancel {
            self.rename_tab = None;
        } else if save {
            let id = rename.id;
            let title = rename.title.trim().to_owned();
            if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
                session.title = title;
            }
            self.rename_tab = None;
        }
    }

    fn host_editor(&mut self, root: &mut egui::Ui) {
        let groups = self.config.groups.clone();
        let Some(editor) = &mut self.editor else {
            return;
        };
        let mut save = false;
        let mut delete = false;
        let mut cancel = false;
        egui::Window::new(if editor.original_name.is_some() {
            "Edit SSH host"
        } else {
            "Add SSH host"
        })
        .collapsible(false)
        .resizable(false)
        .fixed_size([390.0, 350.0])
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .show(root, |ui| {
            egui::Grid::new("host_editor_grid")
                .num_columns(2)
                .spacing([14.0, 10.0])
                .show(ui, |ui| {
                    let field =
                        |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &'static str| {
                            ui.label(RichText::new(label).color(MUTED));
                            ui.add(
                                egui::TextEdit::singleline(value)
                                    .hint_text(hint)
                                    .margin(Margin::symmetric(8, 5))
                                    .desired_width(240.0),
                            );
                            ui.end_row();
                        };
                    field(ui, "Name", &mut editor.host.name, "prod-web-1");
                    field(ui, "Address", &mut editor.host.address, "Hostname or IP");
                    field(ui, "User", &mut editor.host.user, "Optional");
                    ui.label(RichText::new("Port").color(MUTED));
                    ui.add(egui::DragValue::new(&mut editor.host.port).range(1..=65535));
                    ui.end_row();
                    field(
                        ui,
                        "Identity file",
                        &mut editor.host.identity_file,
                        "Optional",
                    );
                    ui.label(RichText::new("Group").color(MUTED));
                    egui::ComboBox::from_id_salt("host_editor_group")
                        .width(240.0)
                        .selected_text(&editor.host.group)
                        .show_ui(ui, |ui| {
                            for group in &groups {
                                ui.selectable_value(&mut editor.host.group, group.clone(), group);
                            }
                        });
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if editor.original_name.is_some()
                    && ui
                        .add(
                            egui::Button::new(
                                RichText::new(if editor.confirm_delete {
                                    "Confirm delete"
                                } else {
                                    "Delete"
                                })
                                .color(RED),
                            )
                            .corner_radius(7)
                            .min_size(egui::vec2(72.0, 30.0)),
                        )
                        .clicked()
                {
                    if editor.confirm_delete {
                        delete = true;
                    } else {
                        editor.confirm_delete = true;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            !editor.host.name.trim().is_empty(),
                            theme::primary_button("Save", 72.0),
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    if ui.add(theme::secondary_button("Cancel", 72.0)).clicked() {
                        cancel = true;
                    }
                });
            });
        });

        if cancel {
            self.editor = None;
        } else if delete {
            let name = editor.original_name.clone().unwrap_or_default();
            self.config.hosts.retain(|host| host.name != name);
            self.config.host_separators.remove(&name);
            self.selected_hosts.remove(&name);
            self.editor = None;
            self.save_config();
        } else if save {
            let mut host = editor.host.clone();
            host.name = host.name.trim().to_owned();
            host.address = host.address.trim().to_owned();
            host.user = host.user.trim().to_owned();
            host.identity_file = host.identity_file.trim().to_owned();
            host.group = host.group.trim().to_owned();
            if host.group.is_empty() {
                host.group = self.move_group.clone();
            }
            self.config.add_group(host.group.clone());
            if let Some(original) = editor.original_name.clone() {
                if original != host.name
                    && self.config.hosts.iter().any(|item| item.name == host.name)
                {
                    self.message = Some(format!("A host named {} already exists.", host.name));
                    return;
                }
                if let Some(existing) = self
                    .config
                    .hosts
                    .iter_mut()
                    .find(|item| item.name == original)
                {
                    // Edited connection details replace the SSH config alias.
                    if existing.address != host.address
                        || existing.user != host.user
                        || existing.port != host.port
                        || existing.identity_file != host.identity_file
                    {
                        host.alias.clear();
                    }
                    *existing = host.clone();
                }
                if original != host.name {
                    if self.config.host_separators.remove(&original) {
                        self.config.host_separators.insert(host.name.clone());
                    }
                    if self.selected_hosts.remove(&original) {
                        self.selected_hosts.insert(host.name.clone());
                    }
                }
                for session in &mut self.sessions {
                    session.replace_host(&original, &host);
                }
            } else if self.config.hosts.iter().any(|item| item.name == host.name) {
                self.message = Some(format!("A host named {} already exists.", host.name));
                return;
            } else {
                self.config.hosts.push(host);
            }
            self.editor = None;
            self.save_config();
        }
    }

    fn group_editor(&mut self, root: &mut egui::Ui) {
        let Some(group) = &mut self.new_group else {
            return;
        };
        let mut create = false;
        let mut cancel = false;
        egui::Window::new("New sidebar header")
            .collapsible(false)
            .resizable(false)
            .fixed_size([310.0, 140.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(root, |ui| {
                ui.add(
                    egui::TextEdit::singleline(group)
                        .hint_text("Header name")
                        .margin(Margin::symmetric(8, 5))
                        .desired_width(260.0),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add_enabled(
                                !group.trim().is_empty(),
                                theme::primary_button("Create", 72.0),
                            )
                            .clicked()
                        {
                            create = true;
                        }
                        if ui.add(theme::secondary_button("Cancel", 72.0)).clicked() {
                            cancel = true;
                        }
                    });
                });
            });

        if cancel {
            self.new_group = None;
        } else if create {
            let group = group.trim().to_owned();
            if self
                .config
                .groups
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&group))
            {
                self.message = Some(format!("A header named {group} already exists."));
            } else {
                self.config.add_group(group.clone());
                self.move_group = group;
                self.save_config();
            }
            self.new_group = None;
        }
    }

    fn command_history(&mut self, root: &mut egui::Ui) {
        if !self.command_history_open {
            return;
        }
        let Some(id) = self.active_session else {
            self.command_history_open = false;
            return;
        };
        let Some(session) = self.sessions.iter().find(|session| session.id == id) else {
            self.command_history_open = false;
            return;
        };
        let remote = session.is_remote();
        let alternate_screen = session.screen().alternate_screen();
        let bracketed_paste = session.screen().bracketed_paste();
        let query = self.command_history_search.to_ascii_lowercase();
        let commands: Vec<_> = session
            .commands()
            .iter()
            .rev()
            .filter(|command| query.is_empty() || command.to_ascii_lowercase().contains(&query))
            .cloned()
            .collect();
        let mut open = true;
        let mut paste = None;

        egui::Window::new("Session commands")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size([620.0, 440.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(root, |ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.command_history_search)
                        .hint_text("Search commands")
                        .margin(Margin::symmetric(8, 6))
                        .desired_width(f32::INFINITY),
                );
                if self.command_history_focus {
                    search.request_focus();
                    self.command_history_focus = false;
                }
                ui.add_space(4.0);
                if commands.is_empty() {
                    ui.label(if remote {
                        "No recorded commands. Interactive remote commands require shell integration on that server; commands sent by ShellHopper appear here."
                    } else {
                        "No commands have run in this local zsh session yet."
                    });
                    return;
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    for command in commands {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                if ui.button("Copy").clicked() {
                                    ui.copy_text(command.clone());
                                }
                                let safe_multiline = !command.contains('\n') || bracketed_paste;
                                if ui
                                    .add_enabled(
                                        !alternate_screen && safe_multiline,
                                        egui::Button::new("Paste"),
                                    )
                                    .on_disabled_hover_text(if alternate_screen {
                                        "Leave the full-screen program before pasting a command"
                                    } else {
                                        "This shell must enable bracketed paste for multiline commands"
                                    })
                                    .clicked()
                                {
                                    paste = Some(command.clone());
                                }
                            });
                            ui.add(
                                egui::Label::new(RichText::new(&command).monospace())
                                    .selectable(true)
                                    .wrap(),
                            );
                        });
                    }
                });
            });

        self.command_history_open = open;
        if let Some(command) = paste
            && let Some(session) = self.sessions.iter_mut().find(|session| session.id == id)
        {
            paste_command(session, &command, bracketed_paste);
            self.command_history_open = false;
            self.active_session = Some(id);
            self.terminal_focus = Some(id);
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let tab_direction = if ctx.input_mut(|input| {
            input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                Key::OpenBracket,
            )
        }) {
            Some(-1)
        } else if ctx.input_mut(|input| {
            input.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                Key::CloseBracket,
            )
        }) {
            Some(1)
        } else {
            None
        };
        if let Some(direction) = tab_direction {
            let tabs = self.tab_ids();
            let active = self.active_session.map(|id| self.tab_root(id));
            if let Some(id) = adjacent_tab(&tabs, active, direction) {
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
            }
        }

        let numbered_tab = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ]
        .into_iter()
        .position(|key| ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, key)));
        if let Some(index) = numbered_tab {
            let tabs = self.tab_ids();
            let target = if index == 8 {
                tabs.last()
            } else {
                tabs.get(index)
            };
            if let Some(id) = target.copied() {
                self.active_session = Some(id);
                self.terminal_focus = Some(id);
            }
        }

        if ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::MAC_CMD, Key::L)
                || input.consume_key(egui::Modifiers::COMMAND, Key::L)
        }) {
            self.command_history_open = !self.command_history_open;
            if self.command_history_open {
                self.command_history_search.clear();
                self.command_history_focus = true;
            }
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::F)) {
            self.terminal_search_open = true;
            self.terminal_search_focus = true;
            self.terminal_search_status = None;
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::K))
            && let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| Some(session.id) == self.active_session)
        {
            session.clear_screen();
        }
        if ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::T)
        }) {
            self.restore_closed_tab();
        } else if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::T)) {
            self.open_local();
        }
        if ctx.input_mut(|input| {
            input.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::SHIFT, Key::D)
        }) {
            self.split_active(SavedSplitDirection::Down);
        } else if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::D)) {
            self.split_active(SavedSplitDirection::Right);
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::W)) {
            if self.command_history_open {
                self.command_history_open = false;
            } else if let Some(id) = self.active_session {
                self.close_pane_or_tab(id);
            }
        }
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, Key::ArrowDown)) {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|session| Some(session.id) == self.active_session)
            {
                session.scroll_to_live();
            }
        }
    }
}

fn saved_tab(session: &TerminalSession) -> SavedTab {
    session
        .host_name()
        .map_or(SavedTab::Local, |host| SavedTab::Ssh {
            host: host.to_owned(),
        })
}

fn saved_pane_layout(layout: &PaneLayout, sessions: &[TerminalSession]) -> Option<SavedTab> {
    match layout {
        PaneLayout::Leaf(id) => sessions
            .iter()
            .find(|session| session.id == *id)
            .map(saved_tab),
        PaneLayout::Split {
            direction,
            ratio,
            first,
            second,
            ..
        } => Some(SavedTab::Split {
            direction: *direction,
            ratio: (*ratio * 1_000.0).round() as u16,
            first: Box::new(saved_pane_layout(first, sessions)?),
            second: Box::new(saved_pane_layout(second, sessions)?),
        }),
    }
}

impl eframe::App for ShellHopperApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        #[cfg(target_os = "macos")]
        if !self.native_window_configured {
            self.native_window_configured = disable_native_titlebar_dragging();
        }
        self.poll();
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quit_confirmed
            && self.live_ssh_count() > 0
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_quit_confirmation = true;
        }
        self.shortcuts(&ctx);
        self.sidebar(root);
        self.tab_bar(root);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(TERMINAL).inner_margin(Margin {
                left: 14,
                right: 6,
                top: 10,
                bottom: 8,
            }))
            .show(root, |ui| {
                self.terminal(ui, &ctx);
            });
        self.command_history(root);
        self.tab_rename_editor(root);
        self.host_editor(root);
        self.group_editor(root);
        self.saved_commands_editor(root);

        if let Some(pending) = &self.pending_upload {
            let mut upload = false;
            let mut cancel = false;
            egui::Window::new("Upload files?")
                .collapsible(false)
                .resizable(false)
                .fixed_size([390.0, 170.0])
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root, |ui| {
                    let count = pending.paths.len();
                    ui.set_min_width(350.0);
                    ui.label(format!(
                        "Upload {count} file{} to {}'s home directory?",
                        if count == 1 { "" } else { "s" },
                        pending.host
                    ));
                    ui.label(
                        RichText::new("Existing files with the same names may be replaced.")
                            .color(MUTED),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(theme::primary_button("Upload", 76.0)).clicked() {
                                upload = true;
                            }
                            if ui.add(theme::secondary_button("Cancel", 72.0)).clicked() {
                                cancel = true;
                            }
                        });
                    });
                });
            if cancel {
                self.pending_upload = None;
            } else if upload {
                let pending = self.pending_upload.take().unwrap();
                match self
                    .sessions
                    .iter()
                    .find(|session| session.id == pending.session_id)
                    .ok_or_else(|| "The SSH pane was closed before the upload started.".to_owned())
                    .and_then(|session| session.upload_files(&pending.host, pending.paths))
                {
                    Ok(receiver) => {
                        self.upload = Some(Upload {
                            host: pending.host,
                            receiver,
                        });
                    }
                    Err(error) => self.message = Some(error),
                }
            }
        }

        if let Some(upload) = &self.upload {
            egui::Window::new("Uploading files")
                .collapsible(false)
                .resizable(false)
                .auto_sized()
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Uploading to {}'s home directory…", upload.host));
                    });
                });
        }

        if self.show_quit_confirmation {
            let count = self.live_ssh_count();
            egui::Window::new("Quit ShellHopper?")
                .collapsible(false)
                .resizable(false)
                .fixed_size([360.0, 165.0])
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root, |ui| {
                    ui.set_min_width(320.0);
                    ui.label(format!(
                        "{count} SSH session{} still connected or connecting.",
                        if count == 1 { " is" } else { "s are" }
                    ));
                    ui.label("Quitting will disconnect them.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(theme::primary_button("Quit", 72.0)).clicked() {
                                self.persist_tab_layout();
                                self.quit_confirmed = true;
                                self.show_quit_confirmation = false;
                                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                            if ui.add(theme::secondary_button("Cancel", 72.0)).clicked() {
                                self.show_quit_confirmation = false;
                            }
                        });
                    });
                });
        }

        #[cfg(target_os = "macos")]
        if self.show_update_confirmation {
            if let Some(version) = self.updater.ready_version().map(str::to_owned) {
                let mut install = false;
                let mut cancel = false;
                egui::Window::new(format!("Update to ShellHopper v{version}?"))
                    .collapsible(false)
                    .resizable(false)
                    .fixed_size([390.0, 185.0])
                    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(root, |ui| {
                        ui.set_min_width(350.0);
                        ui.label("The update is downloaded, signed, and ready to install.");
                        if !self.sessions.is_empty() {
                            ui.label(
                                "Updating will close open terminal processes. Your tab layout will return after relaunch.",
                            );
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(theme::primary_button("Update and Restart", 132.0))
                                    .clicked()
                                {
                                    install = true;
                                }
                                if ui.add(theme::secondary_button("Later", 72.0)).clicked() {
                                    cancel = true;
                                }
                            });
                        });
                    });
                if cancel {
                    self.show_update_confirmation = false;
                } else if install {
                    self.persist_tab_layout();
                    match self.updater.install_and_relaunch() {
                        Ok(()) => {
                            self.quit_confirmed = true;
                            self.show_update_confirmation = false;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(error) => {
                            self.show_update_confirmation = false;
                            self.message = Some(error);
                        }
                    }
                }
            } else {
                self.show_update_confirmation = false;
            }
        }

        if let Some(message) = self.message.clone() {
            egui::Window::new("ShellHopper")
                .collapsible(false)
                .resizable(false)
                .auto_sized()
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root, |ui| {
                    ui.set_min_width(260.0);
                    ui.label(message);
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary_button("OK", 72.0)).clicked() {
                            self.message = None;
                        }
                    });
                });
        }
        self.persist_tab_layout();
        #[cfg(target_os = "macos")]
        ctx.request_repaint_after(std::time::Duration::from_secs(10 * 60));
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.persist_tab_layout();
    }
}

#[derive(Clone, Copy)]
struct TerminalInputState {
    rect: egui::Rect,
    cell_width: f32,
    row_height: f32,
    rows: u16,
    cols: u16,
    application_cursor: bool,
    bracketed_paste: bool,
    alternate_screen: bool,
    mouse_mode: vt100::MouseProtocolMode,
    sgr_mouse: bool,
}

fn handle_terminal_input(
    ctx: &egui::Context,
    session: &mut TerminalSession,
    state: TerminalInputState,
    mut selection: Option<TerminalSelection>,
    history_rows: usize,
) -> bool {
    let mut deleted_selection = false;
    let events = ctx.input(|input| input.events.clone());
    for event in events {
        match event {
            egui::Event::Text(text) => {
                session.scroll_to_live();
                session.send(text.as_bytes());
            }
            egui::Event::Paste(text) => {
                session.scroll_to_live();
                session.send(&paste_sequence(&text, state.bracketed_paste));
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if !state.alternate_screen && modifiers.shift && key == Key::PageUp {
                    session.scroll_page(true);
                    continue;
                }
                if !state.alternate_screen && modifiers.shift && key == Key::PageDown {
                    session.scroll_page(false);
                    continue;
                }
                if matches!(key, Key::Backspace | Key::Delete)
                    && modifiers == egui::Modifiers::NONE
                    && let Some(bytes) = selection.and_then(|selection| {
                        terminal_selection_delete_sequence(
                            session.screen(),
                            selection,
                            history_rows,
                            state.application_cursor,
                        )
                    })
                {
                    session.scroll_to_live();
                    session.send(&bytes);
                    selection = None;
                    deleted_selection = true;
                    continue;
                }
                if let Some(bytes) = terminal_key_sequence(key, modifiers, state.application_cursor)
                {
                    session.scroll_to_live();
                    session.send(&bytes);
                }
            }
            egui::Event::PointerButton {
                pos,
                button,
                pressed,
                modifiers,
            } if state.sgr_mouse
                && !modifiers.shift
                && !modifiers.command
                && button != egui::PointerButton::Secondary =>
            {
                if let Some((col, row)) = terminal_cell(pos, state) {
                    let reports_release = state.mouse_mode != vt100::MouseProtocolMode::Press;
                    if pressed || reports_release {
                        let Some(button) = mouse_button_code(button) else {
                            continue;
                        };
                        session.send(&sgr_mouse_sequence(button, col, row, pressed, modifiers));
                    }
                }
            }
            egui::Event::PointerMoved(pos) if state.sgr_mouse => {
                let (modifiers, held_button) = ctx.input(|input| {
                    let pointer = &input.pointer;
                    let held = [
                        egui::PointerButton::Primary,
                        egui::PointerButton::Middle,
                        egui::PointerButton::Secondary,
                    ]
                    .into_iter()
                    .find(|button| pointer.button_down(*button));
                    (input.modifiers, held)
                });
                let report = match state.mouse_mode {
                    vt100::MouseProtocolMode::AnyMotion => true,
                    vt100::MouseProtocolMode::ButtonMotion => held_button.is_some(),
                    _ => false,
                };
                if report
                    && !modifiers.shift
                    && !modifiers.command
                    && let Some((col, row)) = terminal_cell(pos, state)
                {
                    let button = held_button.and_then(mouse_button_code).unwrap_or(3) + 32;
                    session.send(&sgr_mouse_sequence(button, col, row, true, modifiers));
                }
            }
            egui::Event::MouseWheel {
                unit,
                delta,
                modifiers,
                ..
            } => {
                let position = ctx.input(|input| input.pointer.hover_pos());
                let Some((col, row)) = position.and_then(|pos| terminal_cell(pos, state)) else {
                    continue;
                };
                let repeats = wheel_repeats(unit, delta.y, state.row_height);
                if repeats == 0 {
                    continue;
                }
                if state.sgr_mouse && !modifiers.shift {
                    let button = if delta.y > 0.0 { 64 } else { 65 };
                    let bytes = sgr_mouse_sequence(button, col, row, true, modifiers);
                    for _ in 0..repeats {
                        session.send(&bytes);
                    }
                } else if state.alternate_screen
                    && state.mouse_mode == vt100::MouseProtocolMode::None
                {
                    let key = if delta.y > 0.0 {
                        Key::ArrowUp
                    } else {
                        Key::ArrowDown
                    };
                    if let Some(bytes) =
                        terminal_key_sequence(key, egui::Modifiers::NONE, state.application_cursor)
                    {
                        for _ in 0..repeats {
                            session.send(&bytes);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    deleted_selection
}

fn terminal_selection_delete_sequence(
    screen: &vt100::Screen,
    selection: TerminalSelection,
    history_rows: usize,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    if selection.dragging || screen.scrollback() != 0 || screen.alternate_screen() {
        return None;
    }
    let (start, mut end) = selection.range();
    let (screen_row, cursor_col) = screen.cursor_position();
    let cursor = TerminalPoint {
        row: history_rows + usize::from(screen_row),
        col: cursor_col,
    };
    let mut first_row = screen_row;
    while first_row > 0 && screen.row_wrapped(first_row - 1) {
        first_row -= 1;
    }
    let mut last_row = screen_row;
    while last_row + 1 < screen.size().0 && screen.row_wrapped(last_row) {
        last_row += 1;
    }
    let editable_start = history_rows + usize::from(first_row);
    let editable_end = history_rows + usize::from(last_row);
    if start.row < editable_start || end.row > editable_end {
        return None;
    }

    // shortcut: terminal cells approximate the shell buffer; add shell integration if tabs need exact ranges.
    let rendered_end = (first_row..=last_row)
        .rev()
        .find_map(|row| {
            (0..screen.size().1).rev().find_map(|col| {
                screen
                    .cell(row, col)
                    .filter(|cell| !cell.contents().is_empty())
                    .map(|_| TerminalPoint {
                        row: history_rows + usize::from(row),
                        col: col + 1,
                    })
            })
        })
        .unwrap_or(cursor)
        .max(cursor);
    end = end.min(rendered_end);
    if start >= end {
        return None;
    }

    let characters = |from, to| terminal_character_count(screen, history_rows, from, to);
    let delete_count = characters(start, end);
    if delete_count == 0 {
        return None;
    }

    let mut bytes = if end < cursor {
        terminal_key_sequence(Key::ArrowLeft, egui::Modifiers::NONE, application_cursor)?
            .repeat(characters(end, cursor))
    } else {
        terminal_key_sequence(Key::ArrowRight, egui::Modifiers::NONE, application_cursor)?
            .repeat(characters(cursor, end))
    };
    bytes.extend(vec![b'\x7f'; delete_count]);
    Some(bytes)
}

fn terminal_character_count(
    screen: &vt100::Screen,
    history_rows: usize,
    start: TerminalPoint,
    end: TerminalPoint,
) -> usize {
    (start.row..=end.row)
        .map(|row| {
            let screen_row = u16::try_from(row.saturating_sub(history_rows)).unwrap_or(u16::MAX);
            let from = if row == start.row { start.col } else { 0 };
            let to = if row == end.row {
                end.col
            } else {
                screen.size().1
            };
            (from..to)
                .filter(|col| {
                    !screen
                        .cell(screen_row, *col)
                        .is_some_and(vt100::Cell::is_wide_continuation)
                })
                .count()
        })
        .sum()
}

/// Private key sequences bound by the zsh integration, so the editor never
/// depends on the user's own key bindings or paste handling.
const SHELL_SET_BUFFER: &[u8] = b"\x1b[5733~";
const SHELL_ACCEPT_LINE: &[u8] = b"\x1b[5734~";

/// Makes the shell's line buffer exactly `text`, replacing whatever it held.
fn shell_buffer_sequence(text: &str) -> Vec<u8> {
    // Drop escape characters so the text cannot end its own payload early.
    let text = text.replace('\x1b', "");
    let mut bytes = SHELL_SET_BUFFER.to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Puts a command where the user is typing: the command editor at a prompt,
/// otherwise the running program.
fn paste_command(session: &mut TerminalSession, command: &str, bracketed: bool) {
    session.scroll_to_live();
    if session.editor_active() {
        session.insert_command_input(command);
    } else {
        session.send(&paste_sequence(command, bracketed));
    }
}

/// The highlighted part of the command editor, as character offsets.
fn command_editor_selection(ctx: &egui::Context, id: egui::Id) -> Option<std::ops::Range<usize>> {
    let range = egui::TextEdit::load_state(ctx, id)?
        .cursor
        .char_range()?
        .as_sorted_char_range();
    (range.start != range.end).then_some(range.start.0..range.end.0)
}

/// The run of word characters around a cell, for double-click selection.
fn terminal_word_range(screen: &vt100::Screen, row: u16, col: u16) -> Option<(u16, u16)> {
    let is_word = |col: u16| {
        screen.cell(row, col).is_some_and(|cell| {
            cell.is_wide_continuation()
                || cell.contents().chars().next().is_some_and(|character| {
                    character.is_alphanumeric() || "_-./~+@:%#=?&".contains(character)
                })
        })
    };
    if !is_word(col) {
        return None;
    }
    let mut start = col;
    while start > 0 && is_word(start - 1) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < screen.size().1 && is_word(end) {
        end += 1;
    }
    Some((start, end))
}

/// The column where the shell's prompt ends and the command editor begins.
fn command_editor_column(screen: &vt100::Screen, cols: u16) -> u16 {
    let (cursor_row, cursor_col) = screen.cursor_position();
    if cursor_col == cols.saturating_sub(1) {
        (0..cursor_col)
            .rev()
            .find(|&col| {
                screen
                    .cell(cursor_row, col)
                    .is_some_and(|cell| !cell.contents().trim().is_empty())
            })
            .map_or(cursor_col, |col| col + 1)
    } else {
        cursor_col
    }
}

/// Lays the command out on the terminal's own grid, wrapping at the pane edge.
fn command_editor_job(text: &str, indent: f32, wrap_width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.append(
        text,
        indent,
        TextFormat {
            font_id: FontId::monospace(TERM_FONT_SIZE),
            color: Color32::from_rgb(225, 228, 238),
            line_height: Some(TERM_LINE_HEIGHT),
            ..Default::default()
        },
    );
    job.wrap.max_width = wrap_width;
    job.wrap.break_anywhere = true;
    job
}

fn paste_sequence(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    // Drop escape characters so pasted text cannot contain the end-of-paste
    // sequence and have the remainder run as typed input.
    let text = text.replace('\x1b', "");
    let mut bytes = Vec::with_capacity(text.len() + 12);
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

fn saved_commands_error(commands: &[SavedCommand]) -> Option<String> {
    let mut names = BTreeSet::new();
    for saved in commands {
        let name = saved.name.trim();
        let command = saved.command.trim();
        if name.is_empty() || command.is_empty() {
            return Some("Every saved command needs a name and command.".into());
        }
        if command.contains(['\r', '\n']) {
            return Some("Saved commands must be one line.".into());
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Some(format!("A saved command named {name} already exists."));
        }
    }
    None
}

fn terminal_key_sequence(
    key: Key,
    modifiers: egui::Modifiers,
    application_cursor: bool,
) -> Option<Vec<u8>> {
    if modifiers.mac_cmd {
        return None;
    }
    if modifiers.ctrl
        && let Some(byte) = control_byte(key)
    {
        return Some(if modifiers.alt {
            vec![b'\x1b', byte]
        } else {
            vec![byte]
        });
    }

    let modifier =
        1 + u8::from(modifiers.shift) + 2 * u8::from(modifiers.alt) + 4 * u8::from(modifiers.ctrl);
    let modified = modifier > 1;
    let csi_key = |suffix: char| format!("\x1b[1;{modifier}{suffix}").into_bytes();
    let tilde_key = |code: u8| {
        if modified {
            format!("\x1b[{code};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };

    Some(match key {
        Key::Enter => b"\r".to_vec(),
        Key::Tab if modifiers.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Backspace if modifiers.alt => b"\x1b\x7f".to_vec(),
        Key::Backspace => b"\x7f".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::ArrowUp => cursor_key('A', application_cursor, modified, &csi_key),
        Key::ArrowDown => cursor_key('B', application_cursor, modified, &csi_key),
        Key::ArrowRight => cursor_key('C', application_cursor, modified, &csi_key),
        Key::ArrowLeft => cursor_key('D', application_cursor, modified, &csi_key),
        Key::Home => cursor_key('H', application_cursor, modified, &csi_key),
        Key::End => cursor_key('F', application_cursor, modified, &csi_key),
        Key::Insert => tilde_key(2),
        Key::Delete => tilde_key(3),
        Key::PageUp => tilde_key(5),
        Key::PageDown => tilde_key(6),
        Key::F1 => function_key('P', modified, &csi_key),
        Key::F2 => function_key('Q', modified, &csi_key),
        Key::F3 => function_key('R', modified, &csi_key),
        Key::F4 => function_key('S', modified, &csi_key),
        Key::F5 => tilde_key(15),
        Key::F6 => tilde_key(17),
        Key::F7 => tilde_key(18),
        Key::F8 => tilde_key(19),
        Key::F9 => tilde_key(20),
        Key::F10 => tilde_key(21),
        Key::F11 => tilde_key(23),
        Key::F12 => tilde_key(24),
        _ => return None,
    })
}

fn cursor_key(
    suffix: char,
    application_cursor: bool,
    modified: bool,
    csi_key: &impl Fn(char) -> Vec<u8>,
) -> Vec<u8> {
    if modified {
        csi_key(suffix)
    } else if application_cursor {
        format!("\x1bO{suffix}").into_bytes()
    } else {
        format!("\x1b[{suffix}").into_bytes()
    }
}

fn function_key(suffix: char, modified: bool, csi_key: &impl Fn(char) -> Vec<u8>) -> Vec<u8> {
    if modified {
        csi_key(suffix)
    } else {
        format!("\x1bO{suffix}").into_bytes()
    }
}

fn terminal_cell(pos: egui::Pos2, state: TerminalInputState) -> Option<(u16, u16)> {
    if !state.rect.contains(pos) {
        return None;
    }
    let col = ((pos.x - state.rect.left()) / state.cell_width).floor() as u16 + 1;
    let row = ((pos.y - state.rect.top()) / state.row_height).floor() as u16 + 1;
    Some((col.clamp(1, state.cols), row.clamp(1, state.rows)))
}

fn terminal_selection_point(
    pos: egui::Pos2,
    state: TerminalInputState,
    history_rows: usize,
    scrollback: usize,
) -> TerminalPoint {
    let x = (pos.x - state.rect.left()).clamp(0.0, state.cell_width * f32::from(state.cols));
    let col = (x / state.cell_width).round() as u16;
    let row = ((pos.y - state.rect.top()) / state.row_height)
        .floor()
        .clamp(0.0, f32::from(state.rows.saturating_sub(1))) as usize;
    TerminalPoint {
        row: history_rows.saturating_sub(scrollback) + row,
        col: col.min(state.cols),
    }
}

#[derive(Debug, Eq, PartialEq)]
enum TerminalOpenTarget {
    Url(String),
    LocalPath(PathBuf),
}

impl TerminalOpenTarget {
    fn hover_text(&self) -> String {
        match self {
            Self::Url(url) => format!("Command-click to open {url}"),
            Self::LocalPath(path) => format!("Command-click to open {}", path.display()),
        }
    }
}

fn terminal_open_target_at(
    screen: &vt100::Screen,
    row: u16,
    col: u16,
    allow_local_paths: bool,
) -> Option<TerminalOpenTarget> {
    let (_, cols) = screen.size();
    let mut text = String::new();
    let mut clicked_byte = None;
    for cell_col in 0..cols {
        let cell = screen.cell(row, cell_col)?;
        if cell.is_wide_continuation() {
            continue;
        }
        if cell_col == col {
            clicked_byte = Some(text.len());
        }
        text.push_str(if cell.contents().is_empty() {
            " "
        } else {
            cell.contents()
        });
    }

    let clicked_byte = clicked_byte?;
    if text[clicked_byte..].chars().next()?.is_whitespace() {
        return None;
    }
    let start = text[..clicked_byte]
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    let end = text[clicked_byte..]
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
        .map_or(text.len(), |(index, _)| clicked_byte + index);
    terminal_open_target(&text[start..end], allow_local_paths)
}

fn terminal_open_target(token: &str, allow_local_paths: bool) -> Option<TerminalOpenTarget> {
    let token = token
        .trim_matches(|character| matches!(character, '\'' | '"' | '`' | '(' | '[' | '{' | '<'))
        .trim_end_matches(|character| {
            matches!(
                character,
                '\'' | '"' | '`' | ')' | ']' | '}' | '>' | '.' | ',' | ';'
            )
        });
    let lowercase = token.to_ascii_lowercase();
    if lowercase.starts_with("https://") || lowercase.starts_with("http://") {
        return Some(TerminalOpenTarget::Url(token.to_owned()));
    }
    if !allow_local_paths || !(token.starts_with('/') || token.starts_with("~/")) {
        return None;
    }

    let mut candidate = token;
    for _ in 0..=2 {
        let path = if let Some(relative) = candidate.strip_prefix("~/") {
            PathBuf::from(std::env::var_os("HOME")?).join(relative)
        } else {
            PathBuf::from(candidate)
        };
        if let Ok(path) = path.canonicalize() {
            return Some(TerminalOpenTarget::LocalPath(path));
        }
        let (path, suffix) = candidate.rsplit_once(':')?;
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        candidate = path;
    }
    None
}

fn open_terminal_target(ctx: &egui::Context, target: TerminalOpenTarget) -> Result<(), String> {
    match target {
        TerminalOpenTarget::Url(url) => {
            ctx.open_url(egui::OpenUrl::new_tab(url));
            Ok(())
        }
        TerminalOpenTarget::LocalPath(path) => {
            let mut child = Command::new("/usr/bin/open")
                .arg(&path)
                .spawn()
                .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
            thread::spawn(move || {
                let _ = child.wait();
            });
            Ok(())
        }
    }
}

fn mouse_button_code(button: egui::PointerButton) -> Option<u8> {
    match button {
        egui::PointerButton::Primary => Some(0),
        egui::PointerButton::Middle => Some(1),
        egui::PointerButton::Secondary => Some(2),
        egui::PointerButton::Extra1 | egui::PointerButton::Extra2 => None,
    }
}

fn sgr_mouse_sequence(
    button: u8,
    col: u16,
    row: u16,
    pressed: bool,
    modifiers: egui::Modifiers,
) -> Vec<u8> {
    let modifiers =
        4 * u8::from(modifiers.shift) + 8 * u8::from(modifiers.alt) + 16 * u8::from(modifiers.ctrl);
    let suffix = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{};{col};{row}{suffix}", button + modifiers).into_bytes()
}

fn wheel_repeats(unit: egui::MouseWheelUnit, delta: f32, row_height: f32) -> usize {
    if delta == 0.0 {
        return 0;
    }
    match unit {
        egui::MouseWheelUnit::Point => (delta.abs() / row_height).ceil() as usize,
        egui::MouseWheelUnit::Line => delta.abs().ceil() as usize,
        egui::MouseWheelUnit::Page => 3,
    }
    .clamp(1, 6)
}

fn control_byte(key: Key) -> Option<u8> {
    let letter = match key {
        Key::A => b'a',
        Key::B => b'b',
        Key::C => b'c',
        Key::D => b'd',
        Key::E => b'e',
        Key::F => b'f',
        Key::G => b'g',
        Key::H => b'h',
        Key::I => b'i',
        Key::J => b'j',
        Key::K => b'k',
        Key::L => b'l',
        Key::M => b'm',
        Key::N => b'n',
        Key::O => b'o',
        Key::P => b'p',
        Key::Q => b'q',
        Key::R => b'r',
        Key::S => b's',
        Key::T => b't',
        Key::U => b'u',
        Key::V => b'v',
        Key::W => b'w',
        Key::X => b'x',
        Key::Y => b'y',
        Key::Z => b'z',
        _ => return None,
    };
    Some(letter & 0x1f)
}

fn terminal_layout(
    screen: &vt100::Screen,
    search: Option<&str>,
    history_rows: usize,
    selection: Option<&TerminalSelection>,
    show_cursor: bool,
) -> LayoutJob {
    let (rows, cols) = screen.size();
    let cursor = screen.cursor_position();
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.keep_trailing_whitespace = true;
    let mut run = String::new();
    let mut run_format = None;
    let first_row = history_rows.saturating_sub(screen.scrollback());

    for row in 0..rows {
        let search_matches = search
            .map(|query| terminal_row_matches(screen, row, cols, query))
            .unwrap_or_default();
        for col in 0..cols {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }

            let cursor_here = show_cursor
                && screen.scrollback() == 0
                && !screen.hide_cursor()
                && cursor == (row, col);
            let mut foreground = terminal_color(cell.fgcolor(), Color32::from_rgb(225, 228, 238));
            let mut background = terminal_color(cell.bgcolor(), TERMINAL);
            if cell.bold() {
                foreground = brighten(foreground);
            } else if cell.dim() {
                foreground = dim(foreground);
            }
            if cell.inverse() || cursor_here {
                std::mem::swap(&mut foreground, &mut background);
            }
            if search_matches
                .get(usize::from(col))
                .copied()
                .unwrap_or(false)
            {
                foreground = Color32::from_rgb(26, 29, 36);
                background = Color32::from_rgb(234, 190, 78);
            }
            if selection
                .is_some_and(|selection| selection.contains(first_row + usize::from(row), col))
            {
                foreground = Color32::WHITE;
                background = Color32::from_rgb(50, 96, 151);
            }

            let format = TextFormat {
                font_id: FontId::monospace(TERM_FONT_SIZE),
                line_height: Some(TERM_LINE_HEIGHT),
                color: foreground,
                background,
                italics: cell.italic(),
                underline: if cell.underline() {
                    Stroke::new(1.0, foreground)
                } else {
                    Stroke::NONE
                },
                ..Default::default()
            };
            let text = if cell.contents().is_empty() {
                " "
            } else {
                cell.contents()
            };
            append_terminal_run(&mut job, &mut run, &mut run_format, text, format);
        }
        if row + 1 < rows {
            append_terminal_run(
                &mut job,
                &mut run,
                &mut run_format,
                "\n",
                TextFormat {
                    font_id: FontId::monospace(TERM_FONT_SIZE),
                    line_height: Some(TERM_LINE_HEIGHT),
                    color: Color32::from_rgb(225, 228, 238),
                    background: TERMINAL,
                    ..Default::default()
                },
            );
        }
    }
    if let Some(format) = run_format {
        job.append(&run, 0.0, format);
    }
    job
}

fn terminal_row_matches(screen: &vt100::Screen, row: u16, cols: u16, query: &str) -> Vec<bool> {
    if query.is_empty() {
        return vec![false; usize::from(cols)];
    }
    let mut text = String::new();
    let mut spans = Vec::new();
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let start = text.len();
        text.push_str(if cell.contents().is_empty() {
            " "
        } else {
            cell.contents()
        });
        spans.push((start, text.len(), usize::from(col)));
    }

    let text = text.to_ascii_lowercase();
    let query = query.to_ascii_lowercase();
    let mut matches = vec![false; usize::from(cols)];
    let mut from = 0;
    while from < text.len() {
        let Some(relative_start) = text[from..].find(&query) else {
            break;
        };
        let start = from + relative_start;
        let end = start + query.len();
        for &(span_start, span_end, col) in &spans {
            if span_start < end && span_end > start {
                matches[col] = true;
            }
        }
        from = end.max(start + 1);
    }
    matches
}

fn append_terminal_run(
    job: &mut LayoutJob,
    run: &mut String,
    run_format: &mut Option<TextFormat>,
    text: &str,
    format: TextFormat,
) {
    if run_format.as_ref() != Some(&format) {
        if let Some(previous) = run_format.take() {
            job.append(run, 0.0, previous);
            run.clear();
        }
        *run_format = Some(format);
    }
    run.push_str(text);
}

fn terminal_color(color: vt100::Color, default: Color32) -> Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(red, green, blue) => Color32::from_rgb(red, green, blue),
        vt100::Color::Idx(index @ 0..=15) => ANSI_COLORS[index as usize],
        vt100::Color::Idx(index @ 16..=231) => {
            let index = index - 16;
            Color32::from_rgb(
                color_cube(index / 36),
                color_cube((index / 6) % 6),
                color_cube(index % 6),
            )
        }
        vt100::Color::Idx(index) => {
            let gray = 8 + (index - 232) * 10;
            Color32::from_gray(gray)
        }
    }
}

fn color_cube(value: u8) -> u8 {
    if value == 0 { 0 } else { 55 + value * 40 }
}

fn brighten(color: Color32) -> Color32 {
    Color32::from_rgb(
        color.r().saturating_add(35),
        color.g().saturating_add(35),
        color.b().saturating_add(35),
    )
}

fn dim(color: Color32) -> Color32 {
    let scale = |channel: u8| (u16::from(channel) * 2 / 3) as u8;
    Color32::from_rgb(scale(color.r()), scale(color.g()), scale(color.b()))
}

const ANSI_COLORS: [Color32; 16] = [
    Color32::from_rgb(0, 0, 0),
    Color32::from_rgb(205, 49, 49),
    Color32::from_rgb(13, 188, 121),
    Color32::from_rgb(229, 229, 16),
    Color32::from_rgb(36, 114, 200),
    Color32::from_rgb(188, 63, 188),
    Color32::from_rgb(17, 168, 205),
    Color32::from_rgb(229, 229, 229),
    Color32::from_rgb(102, 102, 102),
    Color32::from_rgb(241, 76, 76),
    Color32::from_rgb(35, 209, 139),
    Color32::from_rgb(245, 245, 67),
    Color32::from_rgb(59, 142, 234),
    Color32::from_rgb(214, 112, 214),
    Color32::from_rgb(41, 184, 219),
    Color32::from_rgb(255, 255, 255),
];

#[cfg(test)]
mod terminal_color_tests {
    use super::*;

    #[test]
    fn maps_ansi_and_truecolor() {
        assert_eq!(
            terminal_color(vt100::Color::Idx(1), TERMINAL),
            ANSI_COLORS[1]
        );
        assert_eq!(
            terminal_color(vt100::Color::Rgb(1, 2, 3), TERMINAL),
            Color32::from_rgb(1, 2, 3)
        );
    }

    #[test]
    fn nested_panes_split_and_collapse() {
        let mut layout = PaneLayout::Leaf(1);
        assert!(layout.split_leaf(1, 2, 10, SavedSplitDirection::Right));
        assert!(layout.split_leaf(2, 3, 11, SavedSplitDirection::Down));
        assert!(layout.contains(1));
        assert!(layout.contains(2));
        assert!(layout.contains(3));

        let (layout, focus) = layout.remove_leaf(2);
        let layout = layout.unwrap();
        assert_eq!(focus, Some(3));
        assert!(layout.contains(1));
        assert!(!layout.contains(2));
        assert!(layout.contains(3));
    }

    #[test]
    fn highlights_each_search_match() {
        let mut parser = vt100::Parser::new(2, 24, 10);
        parser.process(b"alpha beta alpha");

        let matches = terminal_row_matches(parser.screen(), 0, 24, "ALPHA");

        assert!(matches[..5].iter().all(|matched| *matched));
        assert!(matches[11..16].iter().all(|matched| *matched));
        assert!(matches[5..11].iter().all(|matched| !matched));
    }

    #[test]
    fn terminal_selection_normalizes_backwards_drags() {
        let selection = TerminalSelection {
            session_id: 1,
            anchor: TerminalPoint { row: 4, col: 8 },
            head: TerminalPoint { row: 2, col: 3 },
            cols: 80,
            dragging: false,
        };

        assert_eq!(
            selection.range(),
            (
                TerminalPoint { row: 2, col: 3 },
                TerminalPoint { row: 4, col: 8 }
            )
        );
        assert!(selection.contains(3, 0));
        assert!(!selection.contains(4, 8));
    }

    #[test]
    fn deletes_highlighted_text_around_the_terminal_cursor() {
        let mut parser = vt100::Parser::new(2, 8, 0);
        parser.process(b"$ abcdefghi\x1b[2D");
        let selection = TerminalSelection {
            session_id: 1,
            anchor: TerminalPoint { row: 0, col: 6 },
            head: TerminalPoint { row: 1, col: 2 },
            cols: 8,
            dragging: false,
        };

        assert_eq!(
            terminal_selection_delete_sequence(parser.screen(), selection, 0, false),
            Some(b"\x1b[C\x7f\x7f\x7f\x7f".to_vec())
        );
    }

    #[test]
    fn finds_command_click_targets_without_opening_remote_paths() {
        assert_eq!(
            terminal_open_target("(https://example.com/docs).", false),
            Some(TerminalOpenTarget::Url("https://example.com/docs".into()))
        );
        assert_eq!(terminal_open_target("/tmp:12:4", false), None);
        assert_eq!(
            terminal_open_target("/tmp:12:4", true),
            Some(TerminalOpenTarget::LocalPath(
                PathBuf::from("/tmp").canonicalize().unwrap()
            ))
        );
        assert_eq!(
            terminal_open_target("/shellhopper-file-that-does-not-exist", true),
            None
        );
    }

    #[test]
    fn finds_the_target_under_the_clicked_terminal_cell() {
        let mut parser = vt100::Parser::new(1, 48, 0);
        parser.process(b"see https://example.com/docs for details");

        assert_eq!(
            terminal_open_target_at(parser.screen(), 0, 10, false),
            Some(TerminalOpenTarget::Url("https://example.com/docs".into()))
        );
        assert_eq!(terminal_open_target_at(parser.screen(), 0, 1, false), None);
    }

    #[test]
    fn uses_application_and_modified_key_sequences() {
        assert_eq!(
            terminal_key_sequence(Key::ArrowUp, egui::Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            terminal_key_sequence(Key::ArrowLeft, egui::Modifiers::CTRL, false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            terminal_key_sequence(Key::F5, egui::Modifiers::ALT, false),
            Some(b"\x1b[15;3~".to_vec())
        );
    }

    #[test]
    fn command_editor_keeps_pasted_line_breaks_on_the_terminal_grid() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("editor");
        let mut text = String::new();
        let pasted =
            "aws elbv2 describe-load-balancers \\\n  --region ca-central-1 \\\n  --output table";
        for events in [vec![], vec![egui::Event::Paste(pasted.into())], vec![]] {
            let input = egui::RawInput {
                events,
                ..Default::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, _: f32| {
                    let job = command_editor_job(text.as_str(), 40.0, 2000.0);
                    ui.ctx().fonts_mut(|fonts| fonts.layout_job(job))
                };
                let editor = ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .id(id)
                        .layouter(&mut layouter)
                        .desired_rows(1),
                );
                editor.request_focus();
            });
            output.textures_delta.clear();
        }

        assert_eq!(text, pasted);
        let layout = |text: &str, indent: f32, width: f32| {
            let job = command_editor_job(text, indent, width);
            ctx.fonts_mut(|fonts| fonts.layout_job(job))
        };
        assert_eq!(layout(&text, 40.0, 2000.0).size().y, 3.0 * TERM_LINE_HEIGHT);
        assert!(layout("", 40.0, 2000.0).size().y <= TERM_LINE_HEIGHT);

        let cursor_x = |text: &str, index: usize| {
            layout(text, 40.0, 2000.0)
                .pos_from_cursor(egui::text::CCursor::new(index))
                .left()
        };
        assert_eq!(cursor_x("ab", 0), 40.0);
        assert_eq!(cursor_x("ab\ncd", 3), 0.0);

        let font = FontId::monospace(TERM_FONT_SIZE);
        let cell = ctx.fonts_mut(|fonts| fonts.glyph_width(&font, 'M'));
        let full = "x".repeat(20);
        assert_eq!(layout(&full, 5.0 * cell, 25.0 * cell + 0.5).rows.len(), 1);
        assert_eq!(layout(&full, 5.0 * cell, 24.0 * cell + 0.5).rows.len(), 2);
        assert_eq!(layout(&full, 0.0, f32::INFINITY).rows.len(), 1);
    }

    #[test]
    fn double_click_selects_the_word_under_the_pointer() {
        let mut parser = vt100::Parser::new(2, 40, 0);
        parser.process(b"cat ~/notes/todo.txt | grep -i 'fix me'");
        let screen = parser.screen();

        assert_eq!(terminal_word_range(screen, 0, 1), Some((0, 3)));
        assert_eq!(terminal_word_range(screen, 0, 9), Some((4, 20)));
        assert_eq!(terminal_word_range(screen, 0, 3), None);
        assert_eq!(terminal_word_range(screen, 0, 21), None);
    }

    #[test]
    fn editor_text_replaces_the_shell_buffer_without_escapes() {
        assert_eq!(
            shell_buffer_sequence("echo \x1b[201~hi"),
            b"\x1b[5733~echo [201~hi\x1b[201~"
        );
        assert_eq!(shell_buffer_sequence(""), b"\x1b[5733~\x1b[201~");
    }

    #[test]
    fn wraps_bracketed_paste_and_encodes_sgr_mouse() {
        assert_eq!(
            paste_sequence("one\ntwo", true),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
        assert_eq!(
            paste_sequence("safe\x1b[201~rm -rf x\n", true),
            b"\x1b[200~safe[201~rm -rf x\n\x1b[201~"
        );
        assert_eq!(
            dim(Color32::from_rgb(225, 128, 0)),
            Color32::from_rgb(150, 85, 0)
        );
        assert_eq!(
            sgr_mouse_sequence(0, 12, 4, true, egui::Modifiers::CTRL),
            b"\x1b[<16;12;4M"
        );
        assert_eq!(
            sgr_mouse_sequence(0, 12, 4, false, egui::Modifiers::NONE),
            b"\x1b[<0;12;4m"
        );
    }

    #[test]
    fn saved_commands_require_unique_single_line_entries() {
        let valid = SavedCommand {
            name: "Restart queue".into(),
            command: "sudo systemctl restart queue".into(),
        };
        assert_eq!(saved_commands_error(std::slice::from_ref(&valid)), None);
        assert!(
            saved_commands_error(&[
                valid.clone(),
                SavedCommand {
                    name: "restart QUEUE".into(),
                    command: "true".into(),
                },
            ])
            .is_some()
        );
        assert!(
            saved_commands_error(&[SavedCommand {
                name: "Two lines".into(),
                command: "first\nsecond".into(),
            }])
            .is_some()
        );
    }

    #[test]
    fn moves_tabs_before_and_after_targets() {
        let mut tabs = vec![1, 2, 3, 4];

        assert!(move_item(&mut tabs, 0, 2, true));
        assert_eq!(tabs, [2, 3, 1, 4]);
        assert!(move_item(&mut tabs, 3, 0, false));
        assert_eq!(tabs, [4, 2, 3, 1]);
    }

    #[test]
    fn adjacent_tabs_wrap_in_both_directions() {
        let tabs = [10, 20, 30];
        assert_eq!(adjacent_tab(&tabs, Some(10), -1), Some(30));
        assert_eq!(adjacent_tab(&tabs, Some(30), 1), Some(10));
        assert_eq!(adjacent_tab(&[], None, 1), None);
    }
}
