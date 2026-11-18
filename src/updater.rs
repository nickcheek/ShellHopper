use eframe::egui::Context;
use std::{
    env,
    ffi::CString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const RELEASE_API: &str = "https://api.github.com/repos/nickcheek/ShellHopper/releases/latest";
const RELEASES_URL: &str = "https://github.com/nickcheek/ShellHopper/releases/download";
const APPLE_TEAM_ID: &str = "MP2BU74JC2";
const CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const OLD_BUNDLE_PREFIX: &str = ".ShellHopper-update-";

pub struct Updater {
    sender: Sender<UpdateEvent>,
    receiver: Receiver<UpdateEvent>,
    state: UpdateState,
    last_check: Option<Instant>,
    ctx: Context,
}

enum UpdateState {
    Idle,
    Checking,
    Downloading(String),
    Ready(PreparedUpdate),
}

enum UpdateEvent {
    Downloading(String),
    Ready(PreparedUpdate),
    ReadyForInstall(PreparedUpdate),
    RefreshFailed {
        update: PreparedUpdate,
        error: String,
    },
    Current {
        manual: bool,
    },
    Failed {
        error: String,
        manual: bool,
    },
}

pub enum UpdateNotice {
    Current,
    Ready,
    Failed(String),
}

struct PreparedUpdate {
    version: String,
    installed_app: PathBuf,
    staged_app: PathBuf,
}

impl Updater {
    pub fn new(ctx: &Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        cleanup_old_bundles();
        Self {
            sender,
            receiver,
            state: UpdateState::Idle,
            last_check: None,
            ctx: ctx.clone(),
        }
    }

    pub fn check(&mut self, manual: bool) {
        if !matches!(self.state, UpdateState::Idle) {
            return;
        }
        self.state = UpdateState::Checking;
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let event = match prepare_update(&sender, &ctx, env!("CARGO_PKG_VERSION")) {
                Ok(Some(update)) => UpdateEvent::Ready(update),
                Ok(None) => UpdateEvent::Current { manual },
                Err(error) => UpdateEvent::Failed { error, manual },
            };
            let _ = sender.send(event);
            ctx.request_repaint();
        });
    }

    pub fn check_if_due(&mut self) {
        if matches!(self.state, UpdateState::Idle)
            && self
                .last_check
                .is_none_or(|last_check| last_check.elapsed() >= CHECK_INTERVAL)
        {
            self.check(false);
        }
    }

    pub fn poll(&mut self) -> Option<UpdateNotice> {
        let mut notice = None;
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                UpdateEvent::Downloading(version) => self.state = UpdateState::Downloading(version),
                UpdateEvent::Ready(update) => {
                    self.last_check = Some(Instant::now());
                    self.state = UpdateState::Ready(update);
                }
                UpdateEvent::ReadyForInstall(update) => {
                    self.last_check = Some(Instant::now());
                    self.state = UpdateState::Ready(update);
                    notice = Some(UpdateNotice::Ready);
                }
                UpdateEvent::RefreshFailed { update, error } => {
                    self.last_check = Some(Instant::now());
                    self.state = UpdateState::Ready(update);
                    notice = Some(UpdateNotice::Failed(error));
                }
                UpdateEvent::Current { manual } => {
                    self.last_check = Some(Instant::now());
                    self.state = UpdateState::Idle;
                    if manual {
                        notice = Some(UpdateNotice::Current);
                    }
                }
                UpdateEvent::Failed { error, manual } => {
                    self.last_check = Some(Instant::now());
                    self.state = UpdateState::Idle;
                    if manual {
                        notice = Some(UpdateNotice::Failed(error));
                    }
                }
            }
        }
        notice
    }

    pub fn ready_version(&self) -> Option<&str> {
        match &self.state {
            UpdateState::Ready(update) => Some(&update.version),
            _ => None,
        }
    }

    pub fn activity_label(&self) -> Option<String> {
        match &self.state {
            UpdateState::Checking => Some("Checking for updates…".into()),
            UpdateState::Downloading(version) => Some(format!("Downloading v{version}…")),
            UpdateState::Idle | UpdateState::Ready(_) => None,
        }
    }

    pub fn prepare_to_install(&mut self) {
        let UpdateState::Ready(current) = std::mem::replace(&mut self.state, UpdateState::Idle)
        else {
            return;
        };
        self.state = UpdateState::Checking;
        let current_version = current.version.clone();
        let sender = self.sender.clone();
        let ctx = self.ctx.clone();
        thread::spawn(move || {
            let event = match prepare_update(&sender, &ctx, &current_version) {
                Ok(Some(update)) => {
                    let _ = fs::remove_dir_all(&current.staged_app);
                    UpdateEvent::ReadyForInstall(update)
                }
                Ok(None) => UpdateEvent::ReadyForInstall(current),
                Err(error) => UpdateEvent::RefreshFailed {
                    update: current,
                    error,
                },
            };
            let _ = sender.send(event);
            ctx.request_repaint();
        });
    }

    pub fn install_and_relaunch(&mut self) -> Result<(), String> {
        let UpdateState::Ready(update) = std::mem::replace(&mut self.state, UpdateState::Idle)
        else {
            return Err("The downloaded update is no longer available.".into());
        };
        if let Err(error) = update.install_and_relaunch() {
            self.state = UpdateState::Ready(update);
            return Err(error);
        }
        Ok(())
    }
}

impl PreparedUpdate {
    fn install_and_relaunch(&self) -> Result<(), String> {
        atomic_swap(&self.staged_app, &self.installed_app)?;
        if let Err(error) = spawn_relauncher(&self.installed_app) {
            let _ = atomic_swap(&self.staged_app, &self.installed_app);
            return Err(error);
        }
        Ok(())
    }
}

fn prepare_update(
    sender: &Sender<UpdateEvent>,
    ctx: &Context,
    current_version: &str,
) -> Result<Option<PreparedUpdate>, String> {
    let installed_app = installed_bundle()?;
    let temp = TempDirectory::new()?;
    let release_json = temp.path.join("release.json");
    download(RELEASE_API, &release_json, 60, true)?;
    let release = fs::read_to_string(&release_json)
        .map_err(|error| format!("Could not read the GitHub release response: {error}"))?;
    let tag = json_string_field(&release, "tag_name")
        .ok_or_else(|| "GitHub's release response did not include a version.".to_owned())?;
    let version = tag.strip_prefix('v').unwrap_or(&tag);
    if !is_newer_version(version, current_version)? {
        return Ok(None);
    }

    let _ = sender.send(UpdateEvent::Downloading(version.to_owned()));
    ctx.request_repaint();
    let asset_name = format!("ShellHopper-{version}.dmg");
    let asset_url = format!("{RELEASES_URL}/{tag}/{asset_name}");
    let dmg = temp.path.join(&asset_name);
    let checksum = temp.path.join(format!("{asset_name}.sha256"));
    download(&format!("{asset_url}.sha256"), &checksum, 60, false)?;
    download(&asset_url, &dmg, 600, false)?;
    verify_checksum(&dmg, &checksum)?;
    verify_gatekeeper(&dmg, "open")?;

    let mountpoint = temp.path.join("mount");
    fs::create_dir(&mountpoint)
        .map_err(|error| format!("Could not create the update mount point: {error}"))?;
    mount_dmg(&dmg, &mountpoint)?;
    let source_app = mountpoint.join("ShellHopper.app");
    let staged_app = staging_path(&installed_app)?;
    let copy_result = copy_bundle(&source_app, &staged_app);
    let unmount_result = unmount_dmg(&mountpoint);
    if let Err(error) = copy_result {
        let _ = fs::remove_dir_all(&staged_app);
        return Err(error);
    }
    if let Err(error) = unmount_result {
        let _ = fs::remove_dir_all(&staged_app);
        return Err(error);
    }

    let verification = verify_staged_app(&staged_app, version);
    if let Err(error) = verification {
        let _ = fs::remove_dir_all(&staged_app);
        return Err(error);
    }
    Ok(Some(PreparedUpdate {
        version: version.to_owned(),
        installed_app,
        staged_app,
    }))
}

fn download(url: &str, destination: &Path, timeout: u64, api: bool) -> Result<(), String> {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--tlsv1.2",
        "--max-time",
        &timeout.to_string(),
        "--user-agent",
        concat!("ShellHopper/", env!("CARGO_PKG_VERSION")),
        "--output",
    ]);
    command.arg(destination);
    if api {
        command.args([
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
        ]);
    }
    command.arg(url);
    run(&mut command, "download the update")
}

fn verify_checksum(dmg: &Path, checksum: &Path) -> Result<(), String> {
    let expected = fs::read_to_string(checksum)
        .map_err(|error| format!("Could not read the update checksum: {error}"))?
        .split_whitespace()
        .next()
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| "The update checksum is invalid.".to_owned())?;
    let output = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(dmg)
        .output()
        .map_err(|error| format!("Could not calculate the update checksum: {error}"))?;
    if !output.status.success() {
        return Err("Could not calculate the update checksum.".into());
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if actual != expected {
        return Err("The downloaded update did not match its published checksum.".into());
    }
    Ok(())
}

fn mount_dmg(dmg: &Path, mountpoint: &Path) -> Result<(), String> {
    run(
        Command::new("/usr/bin/hdiutil")
            .args([
                "attach",
                "-mountpoint",
                mountpoint.to_string_lossy().as_ref(),
                "-nobrowse",
                "-noautoopen",
                "-readonly",
                "-autofsck",
            ])
            .arg(dmg),
        "mount the update",
    )
}

fn unmount_dmg(mountpoint: &Path) -> Result<(), String> {
    run(
        Command::new("/usr/bin/hdiutil")
            .arg("detach")
            .arg(mountpoint)
            .arg("-force"),
        "unmount the update",
    )
}

fn copy_bundle(source: &Path, destination: &Path) -> Result<(), String> {
    run(
        Command::new("/usr/bin/ditto").arg(source).arg(destination),
        "stage the update",
    )
}

fn verify_staged_app(app: &Path, version: &str) -> Result<(), String> {
    run(
        Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(format!(
                "-R=certificate leaf[subject.OU] = \"{APPLE_TEAM_ID}\""
            ))
            .arg(app),
        "verify the update's developer signature",
    )?;
    verify_gatekeeper(app, "execute")?;
    let output = Command::new("/usr/libexec/PlistBuddy")
        .args(["-c", "Print :CFBundleShortVersionString"])
        .arg(app.join("Contents/Info.plist"))
        .output()
        .map_err(|error| format!("Could not read the staged app version: {error}"))?;
    let staged_version = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || staged_version != version {
        return Err("The staged app version did not match the GitHub release.".into());
    }
    if !app.join("Contents/MacOS/shellhopper").is_file() {
        return Err("The staged update did not contain the ShellHopper executable.".into());
    }
    Ok(())
}

fn verify_gatekeeper(path: &Path, kind: &str) -> Result<(), String> {
    let mut command = Command::new("/usr/sbin/spctl");
    command.args(["--assess", "--type", kind]);
    if kind == "open" {
        command.args(["--context", "context:primary-signature"]);
    }
    command.arg(path);
    run(&mut command, "verify the update with Gatekeeper")
}

fn run(command: &mut Command, action: &str) -> Result<(), String> {
    let output = command
        .output()
        .map_err(|error| format!("Could not {action}: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if detail.is_empty() {
        format!("Could not {action}.")
    } else {
        format!("Could not {action}: {detail}")
    })
}

fn installed_bundle() -> Result<PathBuf, String> {
    let executable =
        env::current_exe().map_err(|error| format!("Could not locate ShellHopper: {error}"))?;
    executable
        .ancestors()
        .find(|path| path.extension().is_some_and(|extension| extension == "app"))
        .map(Path::to_path_buf)
        .ok_or_else(|| "Automatic updates require ShellHopper to run from an app bundle.".into())
}

fn staging_path(installed_app: &Path) -> Result<PathBuf, String> {
    let parent = installed_app
        .parent()
        .ok_or_else(|| "Could not locate the Applications folder.".to_owned())?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // shortcut: unapplied staged updates are redownloaded after restart; persist metadata if this becomes noisy.
    Ok(parent.join(format!(
        "{OLD_BUNDLE_PREFIX}{}-{nonce}.app",
        std::process::id()
    )))
}

fn atomic_swap(staged: &Path, installed: &Path) -> Result<(), String> {
    const RENAME_SWAP: u32 = 0x0000_0002;
    unsafe extern "C" {
        fn renamex_np(from: *const i8, to: *const i8, flags: u32) -> i32;
    }
    let from = CString::new(staged.as_os_str().as_bytes())
        .map_err(|_| "The staged update path was invalid.".to_owned())?;
    let to = CString::new(installed.as_os_str().as_bytes())
        .map_err(|_| "The installed app path was invalid.".to_owned())?;
    let result = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), RENAME_SWAP) };
    if result == 0 {
        Ok(())
    } else {
        Err(format!(
            "Could not replace ShellHopper: {}",
            std::io::Error::last_os_error()
        ))
    }
}

fn spawn_relauncher(installed_app: &Path) -> Result<(), String> {
    let pid = std::process::id().to_string();
    Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.2; done; exec /usr/bin/open -n \"$2\"",
            "shellhopper-update",
            &pid,
        ])
        .arg(installed_app)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Could not relaunch ShellHopper: {error}"))
}

fn cleanup_old_bundles() {
    let Ok(installed) = installed_bundle() else {
        return;
    };
    let Some(parent) = installed.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let old_enough = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= Duration::from_secs(7 * 24 * 60 * 60));
        if old_enough
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(OLD_BUNDLE_PREFIX) && name.ends_with(".app"))
        {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn json_string_field(json: &str, field: &str) -> Option<String> {
    let start = json.find(&format!("\"{field}\""))?;
    let value = json[start + field.len() + 2..]
        .split_once(':')?
        .1
        .trim_start();
    let value = value.strip_prefix('"')?;
    let end = value.find(['"', '\\'])?;
    (value.as_bytes().get(end) == Some(&b'"')).then(|| value[..end].to_owned())
}

fn is_newer_version(candidate: &str, current: &str) -> Result<bool, String> {
    fn parse(version: &str) -> Option<(u64, u64, u64)> {
        let mut parts = version.split('.');
        let parsed = (
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(parsed)
    }
    let candidate = parse(candidate)
        .ok_or_else(|| format!("GitHub returned an invalid release version: {candidate}"))?;
    let current = parse(current).ok_or_else(|| "ShellHopper's version is invalid.".to_owned())?;
    Ok(candidate > current)
}

struct TempDirectory {
    path: PathBuf,
}

impl TempDirectory {
    fn new() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            env::temp_dir().join(format!("shellhopper-update-{}-{nonce}", std::process::id()));
        fs::create_dir(&path)
            .map_err(|error| format!("Could not create the update folder: {error}"))?;
        Ok(Self { path })
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{is_newer_version, json_string_field};

    #[test]
    fn reads_and_compares_release_versions() {
        let json = r#"{"tag_name":"v0.2.0"}"#;
        assert_eq!(
            json_string_field(json, "tag_name").as_deref(),
            Some("v0.2.0")
        );
        assert!(is_newer_version("0.2.0", "0.1.9").unwrap());
        assert!(!is_newer_version("0.1.9", "0.1.9").unwrap());
    }
}
