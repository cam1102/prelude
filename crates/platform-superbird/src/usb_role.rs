use std::{
    cell::Cell,
    fs,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use fs2::FileExt;
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsbRole {
    Device,
    Host,
}

impl UsbRole {
    fn parse(value: &str) -> Result<Self, UsbRoleError> {
        match value.trim() {
            "device" => Ok(Self::Device),
            "host" => Ok(Self::Host),
            other => Err(UsbRoleError::InvalidRole(other.to_owned())),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlatformPaths {
    pub usb_role: PathBuf,
    pub i2c_bus: PathBuf,
    pub state_dir: PathBuf,
    pub persistent_host: PathBuf,
    pub next_boot_host_trial: PathBuf,
    pub max20332_snapshot: PathBuf,
    pub recovery_required: PathBuf,
    pub health_failures: PathBuf,
    pub daemon_heartbeat: PathBuf,
    pub operation_lock: PathBuf,
    pub upstream_boot_role: PathBuf,
}

impl Default for PlatformPaths {
    fn default() -> Self {
        let state_dir = PathBuf::from("/var/lib/prelude");
        Self {
            usb_role: PathBuf::from("/sys/class/usb_role/ffe09000.usb-role-switch/role"),
            i2c_bus: PathBuf::from("/dev/i2c-2"),
            persistent_host: state_dir.join("persistent-host-enabled"),
            next_boot_host_trial: state_dir.join("host-trial-next-boot"),
            max20332_snapshot: state_dir.join("max20332-device-snapshot"),
            recovery_required: state_dir.join("device-recovery-required"),
            health_failures: state_dir.join("host-health-failures"),
            daemon_heartbeat: state_dir.join("daemon-heartbeat"),
            operation_lock: PathBuf::from("/run/prelude/usb-role.lock"),
            upstream_boot_role: PathBuf::from("/var/lib/superbird-usb-role/boot-role"),
            state_dir,
        }
    }
}

#[derive(Debug, Error)]
pub enum UsbRoleError {
    #[error("USB role I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("USB role helper failed ({status}): {stderr}")]
    Command { status: i32, stderr: String },
    #[error("USB role helper was terminated by a signal")]
    CommandSignaled,
    #[error("process {path} timed out after {timeout:?}")]
    CommandTimeout { path: PathBuf, timeout: Duration },
    #[error("process output waiter for {path} terminated unexpectedly")]
    CommandWaiterFailed { path: PathBuf },
    #[error("unexpected USB role value: {0}")]
    InvalidRole(String),
    #[error("requested timeout {0:?} is outside 30s..=900s")]
    InvalidTimeout(Duration),
    #[error("USB role is {actual:?}, expected {expected:?}")]
    RoleMismatch { expected: UsbRole, actual: UsbRole },
    #[error("USB role did not become {expected:?} within {timeout:?}; last role was {actual:?}")]
    TransitionTimeout {
        expected: UsbRole,
        actual: UsbRole,
        timeout: Duration,
    },
    #[error("USB host trial did not create a live automatic-revert unit")]
    RevertNotScheduled,
    #[error("USB host confirmation did not cancel the automatic-revert unit")]
    RevertStillScheduled,
    #[error(
        "USB role operation failed ({operation}); device-mode rollback also failed ({rollback})"
    )]
    RollbackFailed { operation: String, rollback: String },
    #[error("timed out waiting {timeout:?} for USB-role operation lock {path}")]
    LockTimeout { path: PathBuf, timeout: Duration },
    #[error("persistent USB host mode is already confirmed; run device-mode before a new trial")]
    HostAlreadyConfirmed,
    #[error("a timed USB-host trial is already active")]
    HostTrialActive,
    #[error("a next-boot USB-host trial is already scheduled")]
    NextBootHostTrialPending,
    #[error("the next-boot USB-host trial marker is corrupt or uses an unsupported version")]
    InvalidNextBootHostTrial,
    #[error("USB-device recovery is pending; complete device-mode before a host trial")]
    RecoveryPending,
    #[error("USB-device recovery was not armed before entering host mode")]
    RecoveryNotArmed,
}

const NEXT_BOOT_HOST_TRIAL_VERSION: u8 = 1;
const MAXIMUM_NEXT_BOOT_MARKER_BYTES: u64 = 128;

fn validate_trial_timeout(timeout: Duration) -> Result<(), UsbRoleError> {
    if (Duration::from_secs(30)..=Duration::from_secs(900)).contains(&timeout) {
        Ok(())
    } else {
        Err(UsbRoleError::InvalidTimeout(timeout))
    }
}

fn encode_next_boot_host_trial(timeout: Duration) -> Vec<u8> {
    format!(
        "version={NEXT_BOOT_HOST_TRIAL_VERSION}\ntimeout_secs={}\n",
        timeout.as_secs()
    )
    .into_bytes()
}

fn decode_next_boot_host_trial(contents: &[u8]) -> Result<Duration, UsbRoleError> {
    let text = std::str::from_utf8(contents).map_err(|_| UsbRoleError::InvalidNextBootHostTrial)?;
    let mut lines = text.lines();
    if lines.next() != Some("version=1") {
        return Err(UsbRoleError::InvalidNextBootHostTrial);
    }
    let timeout = lines
        .next()
        .and_then(|line| line.strip_prefix("timeout_secs="))
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .ok_or(UsbRoleError::InvalidNextBootHostTrial)?;
    if lines.next().is_some() || encode_next_boot_host_trial(timeout) != contents {
        return Err(UsbRoleError::InvalidNextBootHostTrial);
    }
    validate_trial_timeout(timeout)?;
    Ok(timeout)
}

pub trait RoleCommand: std::fmt::Debug {
    fn run(&self, arguments: &[String]) -> Result<(), UsbRoleError>;
    fn revert_scheduled(&self) -> Result<bool, UsbRoleError>;
}

#[derive(Debug, Clone)]
pub struct ProcessRoleCommand {
    executable: PathBuf,
    systemctl: PathBuf,
}

impl Default for ProcessRoleCommand {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("/usr/bin/superbird-usb-role"),
            systemctl: PathBuf::from("/usr/bin/systemctl"),
        }
    }
}

impl ProcessRoleCommand {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            systemctl: PathBuf::from("/usr/bin/systemctl"),
        }
    }

    pub fn with_systemctl(executable: impl Into<PathBuf>, systemctl: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            systemctl: systemctl.into(),
        }
    }
}

impl RoleCommand for ProcessRoleCommand {
    fn run(&self, arguments: &[String]) -> Result<(), UsbRoleError> {
        let mut command = Command::new(&self.executable);
        command.env("PRELUDE_ROLE_CONTROLLER", "1").args(arguments);
        let output =
            process_output_with_timeout(&mut command, &self.executable, Duration::from_secs(10))?;
        if output.status.success() {
            return Ok(());
        }
        let Some(status) = output.status.code() else {
            return Err(UsbRoleError::CommandSignaled);
        };
        Err(UsbRoleError::Command {
            status,
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }

    fn revert_scheduled(&self) -> Result<bool, UsbRoleError> {
        let mut command = Command::new(&self.systemctl);
        command.args([
            "is-active",
            "--quiet",
            "superbird-usb-role-revert.timer",
            "superbird-usb-role-revert.service",
        ]);
        let output =
            process_output_with_timeout(&mut command, &self.systemctl, Duration::from_secs(5))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(3 | 4) => Ok(false),
            Some(status) => Err(UsbRoleError::Command {
                status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            }),
            None => Err(UsbRoleError::CommandSignaled),
        }
    }
}

pub fn process_output_with_timeout(
    command: &mut Command,
    executable: &Path,
    timeout: Duration,
) -> Result<Output, UsbRoleError> {
    use std::os::unix::process::CommandExt;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let child = command.spawn().map_err(|source| UsbRoleError::Io {
        path: executable.to_path_buf(),
        source,
    })?;
    let process_group = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let (sender, receiver) = mpsc::sync_channel(1);
    let waiter = thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(timeout) {
        Ok(output) => {
            waiter
                .join()
                .map_err(|_| UsbRoleError::CommandWaiterFailed {
                    path: executable.to_path_buf(),
                })?;
            output.map_err(|source| UsbRoleError::Io {
                path: executable.to_path_buf(),
                source,
            })
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = waiter.join();
            Err(UsbRoleError::CommandWaiterFailed {
                path: executable.to_path_buf(),
            })
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = killpg(Pid::from_raw(process_group), Signal::SIGKILL);
            if receiver.recv_timeout(Duration::from_secs(1)).is_ok() {
                let _ = waiter.join();
            }
            Err(UsbRoleError::CommandTimeout {
                path: executable.to_path_buf(),
                timeout,
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBootOutcome {
    NotConfirmed,
    TimedTrial,
    Activated,
}

#[derive(Debug)]
pub struct UsbRoleManager<R> {
    paths: PlatformPaths,
    command: R,
}

impl<R: RoleCommand> UsbRoleManager<R> {
    pub const fn new(paths: PlatformPaths, command: R) -> Self {
        Self { paths, command }
    }

    pub fn current(&self) -> Result<UsbRole, UsbRoleError> {
        let value = read_to_string(&self.paths.usb_role)?;
        UsbRole::parse(&value)
    }

    pub fn host_trial(&self, timeout: Duration) -> Result<(), UsbRoleError> {
        let transaction = self.begin_transaction()?;
        transaction.prepare_host_trial(timeout)?;
        transaction.host_trial(timeout)
    }

    pub fn schedule_host_trial_next_boot(&self, timeout: Duration) -> Result<(), UsbRoleError> {
        self.begin_transaction()?
            .schedule_host_trial_next_boot(timeout)
    }

    pub fn host_boot(&self) -> Result<HostBootOutcome, UsbRoleError> {
        let transaction = self.begin_transaction()?;
        let outcome = transaction.prepare_host_boot()?;
        match outcome {
            HostBootOutcome::NotConfirmed => transaction.complete_device_recovery()?,
            HostBootOutcome::TimedTrial => {}
            HostBootOutcome::Activated => {
                transaction.host_boot()?;
                transaction.complete_host_activation()?;
            }
        }
        Ok(outcome)
    }

    pub fn automatic_host_boot(&self) -> Result<HostBootOutcome, UsbRoleError> {
        let transaction = self.begin_transaction()?;
        let outcome = transaction.prepare_automatic_host_boot()?;
        match outcome {
            HostBootOutcome::NotConfirmed => transaction.complete_device_recovery()?,
            HostBootOutcome::TimedTrial => {}
            HostBootOutcome::Activated => {
                transaction.host_boot()?;
                transaction.complete_host_activation()?;
            }
        }
        Ok(outcome)
    }

    pub fn confirm_host(&self) -> Result<(), UsbRoleError> {
        self.begin_transaction()?.confirm_host()
    }

    pub fn persistent_host_confirmed(&self) -> bool {
        matches!(fs::read(&self.paths.persistent_host), Ok(contents) if contents == b"confirmed\n")
    }

    pub fn next_boot_host_trial_pending(&self) -> bool {
        matches!(self.next_boot_host_trial_present(), Ok(true))
    }

    pub fn device_mode(&self) -> Result<(), UsbRoleError> {
        let transaction = self.begin_transaction()?;
        transaction.device_mode()?;
        transaction.complete_device_recovery()
    }

    pub fn begin_transaction(&self) -> Result<UsbRoleTransaction<'_, R>, UsbRoleError> {
        Ok(UsbRoleTransaction {
            manager: self,
            _lock: OperationLock::acquire(&self.paths.operation_lock)?,
            host_prepared: Cell::new(false),
        })
    }

    fn require_role(&self, expected: UsbRole) -> Result<(), UsbRoleError> {
        let actual = self.current()?;
        if actual == expected {
            return Ok(());
        }
        Err(UsbRoleError::RoleMismatch { expected, actual })
    }

    fn wait_for_role(&self, expected: UsbRole, timeout: Duration) -> Result<(), UsbRoleError> {
        let deadline = Instant::now() + timeout;
        loop {
            let actual = self.current()?;
            if actual == expected {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(UsbRoleError::TransitionTimeout {
                    expected,
                    actual,
                    timeout,
                });
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub const fn paths(&self) -> &PlatformPaths {
        &self.paths
    }

    fn next_boot_host_trial_present(&self) -> Result<bool, UsbRoleError> {
        match fs::symlink_metadata(&self.paths.next_boot_host_trial) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(UsbRoleError::Io {
                path: self.paths.next_boot_host_trial.clone(),
                source,
            }),
        }
    }

    fn rollback_after<T>(&self, operation: UsbRoleError) -> Result<T, UsbRoleError> {
        let marker_error = create_state(&self.paths.recovery_required, b"required\n").err();
        let rollback = self
            .command
            .run(&["to-device".to_owned()])
            .and_then(|()| self.wait_for_role(UsbRole::Device, Duration::from_secs(5)))
            .and_then(|()| remove_if_exists(&self.paths.persistent_host))
            .and_then(|()| remove_if_exists_durable(&self.paths.next_boot_host_trial));
        match rollback {
            Ok(()) => Err(operation),
            Err(rollback) => Err(UsbRoleError::RollbackFailed {
                operation: operation.to_string(),
                rollback: match marker_error {
                    Some(marker) => {
                        format!("could not persist recovery requirement ({marker}); {rollback}")
                    }
                    None => rollback.to_string(),
                },
            }),
        }
    }
}

#[derive(Debug)]
pub struct UsbRoleTransaction<'a, R> {
    manager: &'a UsbRoleManager<R>,
    _lock: OperationLock,
    host_prepared: Cell<bool>,
}

impl<R: RoleCommand> UsbRoleTransaction<'_, R> {
    pub fn prepare_host_trial(&self, timeout: Duration) -> Result<(), UsbRoleError> {
        validate_trial_timeout(timeout)?;
        if self.manager.persistent_host_confirmed() {
            return Err(UsbRoleError::HostAlreadyConfirmed);
        }
        if self.recovery_required() {
            return Err(UsbRoleError::RecoveryPending);
        }
        self.manager.require_role(UsbRole::Device)?;
        if self.manager.next_boot_host_trial_present()? {
            return Err(UsbRoleError::NextBootHostTrialPending);
        }
        if self.automatic_revert_pending()? {
            return Err(UsbRoleError::HostTrialActive);
        }
        self.arm_device_recovery()?;
        self.host_prepared.set(true);
        Ok(())
    }

    pub fn schedule_host_trial_next_boot(&self, timeout: Duration) -> Result<(), UsbRoleError> {
        validate_trial_timeout(timeout)?;
        if self.manager.persistent_host_confirmed() {
            return Err(UsbRoleError::HostAlreadyConfirmed);
        }
        if self.recovery_required() {
            return Err(UsbRoleError::RecoveryPending);
        }
        self.manager.require_role(UsbRole::Device)?;
        if self.manager.next_boot_host_trial_present()? {
            return Err(UsbRoleError::NextBootHostTrialPending);
        }
        if self.automatic_revert_pending()? {
            return Err(UsbRoleError::HostTrialActive);
        }
        create_state(
            &self.manager.paths.next_boot_host_trial,
            &encode_next_boot_host_trial(timeout),
        )
    }

    pub fn consume_host_trial_next_boot(&self) -> Result<Option<Duration>, UsbRoleError> {
        let result = (|| {
            let Some(contents) =
                take_next_boot_host_trial(&self.manager.paths.next_boot_host_trial)?
            else {
                return Ok(None);
            };
            let timeout = decode_next_boot_host_trial(&contents)?;
            self.prepare_host_trial(timeout)?;
            Ok(Some(timeout))
        })();
        match result {
            Ok(outcome) => Ok(outcome),
            Err(error) => self.manager.rollback_after(error),
        }
    }

    pub fn host_trial(&self, timeout: Duration) -> Result<(), UsbRoleError> {
        validate_trial_timeout(timeout)?;
        if !self.host_prepared.get() || !self.recovery_required() {
            return Err(UsbRoleError::RecoveryNotArmed);
        }
        remove_if_exists(&self.manager.paths.persistent_host)?;
        let arguments = vec![
            "to-host".to_owned(),
            "--revert-secs".to_owned(),
            timeout.as_secs().to_string(),
        ];
        if let Err(error) = self.manager.command.run(&arguments) {
            return self.manager.rollback_after(error);
        }
        match self.manager.command.revert_scheduled() {
            Ok(true) => {}
            Ok(false) => {
                return self
                    .manager
                    .rollback_after(UsbRoleError::RevertNotScheduled);
            }
            Err(error) => return self.manager.rollback_after(error),
        }
        if let Err(error) = self
            .manager
            .wait_for_role(UsbRole::Host, Duration::from_secs(5))
        {
            return self.manager.rollback_after(error);
        }
        Ok(())
    }

    pub fn prepare_host_boot(&self) -> Result<HostBootOutcome, UsbRoleError> {
        remove_if_exists(&self.manager.paths.upstream_boot_role)?;
        if self.recovery_required() {
            if self.automatic_revert_pending()? {
                return Ok(HostBootOutcome::TimedTrial);
            }
            self.device_mode()?;
            return Ok(HostBootOutcome::NotConfirmed);
        }
        if self.manager.next_boot_host_trial_present()? {
            return Ok(HostBootOutcome::TimedTrial);
        }
        if !self.manager.persistent_host_confirmed() {
            self.device_mode()?;
            return Ok(HostBootOutcome::NotConfirmed);
        }
        self.arm_device_recovery()?;
        self.host_prepared.set(true);
        Ok(HostBootOutcome::Activated)
    }

    pub fn prepare_automatic_host_boot(&self) -> Result<HostBootOutcome, UsbRoleError> {
        remove_if_exists(&self.manager.paths.upstream_boot_role)?;
        if self.recovery_required() {
            if self.automatic_revert_pending()? {
                return Ok(HostBootOutcome::TimedTrial);
            }
            self.device_mode()?;
            return Ok(HostBootOutcome::NotConfirmed);
        }
        if self.manager.next_boot_host_trial_present()? {
            return Ok(HostBootOutcome::TimedTrial);
        }
        self.manager.require_role(UsbRole::Device)?;
        self.arm_device_recovery()?;
        self.host_prepared.set(true);
        Ok(HostBootOutcome::Activated)
    }

    pub fn host_boot(&self) -> Result<(), UsbRoleError> {
        if !self.host_prepared.get() || !self.recovery_required() {
            return Err(UsbRoleError::RecoveryNotArmed);
        }
        if let Err(error) = self
            .manager
            .command
            .run(&["to-host".to_owned(), "--no-revert".to_owned()])
        {
            return self.manager.rollback_after(error);
        }
        if let Err(error) = self
            .manager
            .wait_for_role(UsbRole::Host, Duration::from_secs(5))
        {
            return self.manager.rollback_after(error);
        }
        Ok(())
    }

    pub fn confirm_host(&self) -> Result<(), UsbRoleError> {
        if self.manager.next_boot_host_trial_present()? {
            return Err(UsbRoleError::NextBootHostTrialPending);
        }
        self.manager.require_role(UsbRole::Host)?;
        match self.manager.command.revert_scheduled() {
            Ok(true) => {}
            Ok(false) => {
                return self
                    .manager
                    .rollback_after(UsbRoleError::RevertNotScheduled);
            }
            Err(error) => return self.manager.rollback_after(error),
        }
        if let Err(error) = self.manager.command.run(&["confirm".to_owned()]) {
            return self.manager.rollback_after(error);
        }
        match self.manager.command.revert_scheduled() {
            Ok(false) => {}
            Ok(true) => {
                return self
                    .manager
                    .rollback_after(UsbRoleError::RevertStillScheduled);
            }
            Err(error) => return self.manager.rollback_after(error),
        }
        if let Err(error) = self.manager.require_role(UsbRole::Host) {
            return self.manager.rollback_after(error);
        }
        if let Err(error) = create_state(&self.manager.paths.persistent_host, b"confirmed\n") {
            return self.manager.rollback_after(error);
        }
        if let Err(error) = self.complete_host_activation() {
            return self.manager.rollback_after(error);
        }
        Ok(())
    }

    pub fn arm_device_recovery(&self) -> Result<(), UsbRoleError> {
        create_state(&self.manager.paths.recovery_required, b"required\n")
    }

    pub fn device_mode(&self) -> Result<(), UsbRoleError> {
        create_state(&self.manager.paths.recovery_required, b"required\n")?;
        remove_if_exists(&self.manager.paths.upstream_boot_role)?;
        self.manager.command.run(&["to-device".to_owned()])?;
        self.manager
            .wait_for_role(UsbRole::Device, Duration::from_secs(5))?;
        remove_if_exists(&self.manager.paths.persistent_host)?;
        remove_if_exists_durable(&self.manager.paths.next_boot_host_trial)?;
        Ok(())
    }

    pub fn complete_device_recovery(&self) -> Result<(), UsbRoleError> {
        self.manager.require_role(UsbRole::Device)?;
        remove_if_exists(&self.manager.paths.recovery_required)
    }

    pub fn complete_host_activation(&self) -> Result<(), UsbRoleError> {
        self.manager.require_role(UsbRole::Host)?;
        remove_if_exists(&self.manager.paths.recovery_required)
    }

    pub fn recovery_required(&self) -> bool {
        state_is(&self.manager.paths.recovery_required, b"required\n")
    }

    pub fn automatic_revert_pending(&self) -> Result<bool, UsbRoleError> {
        self.manager.command.revert_scheduled()
    }
}

#[derive(Debug)]
struct OperationLock {
    _file: fs::File,
}

impl OperationLock {
    fn acquire(path: &Path) -> Result<Self, UsbRoleError> {
        const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
        Self::acquire_with_timeout(path, LOCK_TIMEOUT)
    }

    fn acquire_with_timeout(path: &Path, timeout: Duration) -> Result<Self, UsbRoleError> {
        const LOCK_RETRY: Duration = Duration::from_millis(10);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| UsbRoleError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|source| UsbRoleError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let deadline = Instant::now() + timeout;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(UsbRoleError::LockTimeout {
                            path: path.to_path_buf(),
                            timeout,
                        });
                    }
                    thread::sleep(LOCK_RETRY);
                }
                Err(source) => {
                    return Err(UsbRoleError::Io {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }
}

fn read_to_string(path: &Path) -> Result<String, UsbRoleError> {
    fs::read_to_string(path).map_err(|source| UsbRoleError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn create_state(path: &Path, contents: &[u8]) -> Result<(), UsbRoleError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| UsbRoleError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("marker");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok::<(), io::Error>(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary);
        return Err(UsbRoleError::Io {
            path: path.to_path_buf(),
            source,
        });
    }
    Ok(())
}

fn state_is(path: &Path, expected: &[u8]) -> bool {
    matches!(fs::read(path), Ok(contents) if contents == expected)
}

fn take_next_boot_host_trial(path: &Path) -> Result<Option<Vec<u8>>, UsbRoleError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(UsbRoleError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let contents =
        if metadata.file_type().is_file() && metadata.len() <= MAXIMUM_NEXT_BOOT_MARKER_BYTES {
            fs::read(path).map_err(|source| UsbRoleError::Io {
                path: path.to_path_buf(),
                source,
            })
        } else {
            Err(UsbRoleError::InvalidNextBootHostTrial)
        };
    remove_if_exists_durable(path)?;
    contents.map(Some)
}

fn remove_if_exists(path: &Path) -> Result<(), UsbRoleError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(UsbRoleError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn remove_if_exists_durable(path: &Path) -> Result<(), UsbRoleError> {
    remove_if_exists(path)?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| UsbRoleError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, cell::RefCell, fs};

    use tempfile::TempDir;

    use super::*;

    #[derive(Debug)]
    struct FakeCommand {
        calls: RefCell<Vec<Vec<String>>>,
        role_path: PathBuf,
        revert_scheduled: Cell<bool>,
        schedule_after_host: bool,
        cancel_works: bool,
        fail_timed_host: bool,
        fail_to_device: bool,
    }

    impl FakeCommand {
        fn new(role_path: PathBuf) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                role_path,
                revert_scheduled: Cell::new(false),
                schedule_after_host: true,
                cancel_works: true,
                fail_timed_host: false,
                fail_to_device: false,
            }
        }
    }

    impl RoleCommand for FakeCommand {
        fn run(&self, arguments: &[String]) -> Result<(), UsbRoleError> {
            self.calls.borrow_mut().push(arguments.to_vec());
            match arguments.first().map(String::as_str) {
                Some("to-host") => {
                    fs::write(&self.role_path, "host\n").expect("update fake role");
                    if arguments.iter().any(|argument| argument == "--revert-secs") {
                        if self.fail_timed_host {
                            return Err(UsbRoleError::Command {
                                status: 1,
                                stderr: "timer creation failed".to_owned(),
                            });
                        }
                        self.revert_scheduled.set(self.schedule_after_host);
                    }
                }
                Some("confirm") if self.cancel_works => self.revert_scheduled.set(false),
                Some("to-device") => {
                    if self.fail_to_device {
                        return Err(UsbRoleError::Command {
                            status: 1,
                            stderr: "device transition failed".to_owned(),
                        });
                    }
                    self.revert_scheduled.set(false);
                    fs::write(&self.role_path, "device\n").expect("update fake role");
                }
                _ => {}
            }
            Ok(())
        }

        fn revert_scheduled(&self) -> Result<bool, UsbRoleError> {
            Ok(self.revert_scheduled.get())
        }
    }

    fn fixture(role: &str) -> (TempDir, PlatformPaths) {
        let directory = TempDir::new().expect("temporary directory");
        let role_path = directory.path().join("role");
        fs::write(&role_path, role).expect("role fixture");
        let state = directory.path().join("state");
        (
            directory,
            PlatformPaths {
                usb_role: role_path,
                i2c_bus: PathBuf::from("/dev/null"),
                persistent_host: state.join("persistent-host-enabled"),
                next_boot_host_trial: state.join("host-trial-next-boot"),
                max20332_snapshot: state.join("max20332-device-snapshot"),
                recovery_required: state.join("device-recovery-required"),
                health_failures: state.join("host-health-failures"),
                daemon_heartbeat: state.join("daemon-heartbeat"),
                operation_lock: state.join("usb-role.lock"),
                upstream_boot_role: state.join("upstream-boot-role"),
                state_dir: state,
            },
        )
    }

    #[test]
    fn boot_is_noop_until_confirmed() {
        let (_directory, paths) = fixture("device\n");
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert_eq!(
            manager.host_boot().expect("boot should be safe"),
            HostBootOutcome::NotConfirmed
        );
        assert_eq!(
            manager.command.calls.borrow().as_slice(),
            &[vec!["to-device".to_owned()]]
        );
    }

    #[test]
    fn boot_activates_a_confirmed_host() {
        let (_directory, paths) = fixture("device\n");
        create_state(&paths.persistent_host, b"confirmed\n").expect("confirmation");
        let recovery = paths.recovery_required.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert_eq!(
            manager.host_boot().expect("confirmed host boot"),
            HostBootOutcome::Activated
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Host);
        assert!(!recovery.exists());
        assert_eq!(
            manager.command.calls.borrow().as_slice(),
            &[vec!["to-host".to_owned(), "--no-revert".to_owned()]]
        );
    }

    #[test]
    fn automatic_image_policy_activates_host_without_confirmation_marker() {
        let (_directory, paths) = fixture("device\n");
        let persistent = paths.persistent_host.clone();
        let recovery = paths.recovery_required.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert_eq!(
            manager.automatic_host_boot().expect("automatic host boot"),
            HostBootOutcome::Activated
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Host);
        assert!(!persistent.exists());
        assert!(!recovery.exists());
        assert_eq!(
            manager.command.calls.borrow().as_slice(),
            &[vec!["to-host".to_owned(), "--no-revert".to_owned()]]
        );
    }

    #[test]
    fn automatic_image_policy_defers_to_a_scheduled_timed_trial() {
        let (_directory, paths) = fixture("device\n");
        create_state(
            &paths.next_boot_host_trial,
            &encode_next_boot_host_trial(Duration::from_secs(300)),
        )
        .expect("scheduled trial");
        let marker = paths.next_boot_host_trial.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert_eq!(
            manager
                .automatic_host_boot()
                .expect("automatic host safeguard"),
            HostBootOutcome::TimedTrial
        );
        assert!(marker.exists());
        assert!(manager.command.calls.borrow().is_empty());
    }

    #[test]
    fn automatic_image_policy_restores_an_interrupted_transition_before_retry() {
        let (_directory, paths) = fixture("host\n");
        create_state(&paths.recovery_required, b"required\n").expect("recovery marker");
        let recovery = paths.recovery_required.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert_eq!(
            manager
                .automatic_host_boot()
                .expect("interrupted transition recovery"),
            HostBootOutcome::NotConfirmed
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert!(!recovery.exists());
        assert_eq!(
            manager.command.calls.borrow().as_slice(),
            &[vec!["to-device".to_owned()]]
        );
    }

    #[test]
    fn trial_has_bounded_timeout() {
        let (_directory, paths) = fixture("device\n");
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        manager
            .host_trial(Duration::from_secs(300))
            .expect("trial should start");
        assert_eq!(
            manager.command.calls.borrow().as_slice(),
            &[vec![
                "to-host".to_owned(),
                "--revert-secs".to_owned(),
                "300".to_owned()
            ]]
        );
        assert!(matches!(
            manager.host_trial(Duration::from_secs(10)),
            Err(UsbRoleError::InvalidTimeout(_))
        ));
    }

    #[test]
    fn next_boot_trial_is_canonical_atomic_and_does_not_touch_hardware() {
        let (_directory, paths) = fixture("device\n");
        let marker = paths.next_boot_host_trial.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        manager
            .schedule_host_trial_next_boot(Duration::from_secs(300))
            .expect("schedule next-boot trial");
        assert_eq!(
            fs::read(&marker).expect("marker"),
            b"version=1\ntimeout_secs=300\n"
        );
        assert!(manager.command.calls.borrow().is_empty());
        assert!(!manager.persistent_host_confirmed());
        assert!(!manager.paths.recovery_required.exists());
        assert!(matches!(
            manager.schedule_host_trial_next_boot(Duration::from_secs(300)),
            Err(UsbRoleError::NextBootHostTrialPending)
        ));
    }

    #[test]
    fn next_boot_trial_refuses_non_device_recovery_confirmation_and_live_trial() {
        let (_directory, paths) = fixture("host\n");
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths.clone(), command);
        assert!(matches!(
            manager.schedule_host_trial_next_boot(Duration::from_secs(300)),
            Err(UsbRoleError::RoleMismatch { .. })
        ));

        fs::write(&paths.usb_role, "device\n").expect("device role");
        create_state(&paths.recovery_required, b"required\n").expect("recovery");
        assert!(matches!(
            manager.schedule_host_trial_next_boot(Duration::from_secs(300)),
            Err(UsbRoleError::RecoveryPending)
        ));
        remove_if_exists(&paths.recovery_required).expect("clear recovery");

        create_state(&paths.persistent_host, b"confirmed\n").expect("confirmation");
        assert!(matches!(
            manager.schedule_host_trial_next_boot(Duration::from_secs(300)),
            Err(UsbRoleError::HostAlreadyConfirmed)
        ));
        remove_if_exists(&paths.persistent_host).expect("clear confirmation");

        manager.command.revert_scheduled.set(true);
        assert!(matches!(
            manager.schedule_host_trial_next_boot(Duration::from_secs(300)),
            Err(UsbRoleError::HostTrialActive)
        ));
        assert!(!paths.next_boot_host_trial.exists());
    }

    #[test]
    fn next_boot_trial_survives_reboot_and_is_consumed_once() {
        let (_directory, paths) = fixture("device\n");
        UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()))
            .schedule_host_trial_next_boot(Duration::from_secs(300))
            .expect("schedule before reboot");

        let manager = UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()));
        let transaction = manager.begin_transaction().expect("boot transaction");
        let timeout = transaction
            .consume_host_trial_next_boot()
            .expect("consume")
            .expect("scheduled trial");
        assert_eq!(timeout, Duration::from_secs(300));
        assert!(!paths.next_boot_host_trial.exists());
        assert!(transaction.recovery_required());
        transaction.host_trial(timeout).expect("start timed trial");
        assert_eq!(manager.current().expect("role"), UsbRole::Host);
        assert!(transaction.automatic_revert_pending().expect("timer"));
        assert!(!manager.persistent_host_confirmed());
        drop(transaction);

        assert_eq!(
            manager.host_boot().expect("persistent boot safeguard"),
            HostBootOutcome::TimedTrial
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Host);

        assert_eq!(
            manager
                .begin_transaction()
                .expect("second boot transaction")
                .consume_host_trial_next_boot()
                .expect("second consume"),
            None
        );
    }

    #[test]
    fn corrupt_next_boot_trial_is_consumed_and_rolls_back_fail_safe() {
        let (_directory, paths) = fixture("device\n");
        create_state(
            &paths.next_boot_host_trial,
            b"version=99\ntimeout_secs=300\n",
        )
        .expect("corrupt marker");
        let manager = UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()));
        let transaction = manager.begin_transaction().expect("boot transaction");

        assert!(matches!(
            transaction.consume_host_trial_next_boot(),
            Err(UsbRoleError::InvalidNextBootHostTrial)
        ));
        assert!(!paths.next_boot_host_trial.exists());
        assert!(transaction.recovery_required());
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert_eq!(
            manager.command.calls.borrow().last(),
            Some(&vec!["to-device".to_owned()])
        );
    }

    #[test]
    fn failed_consumed_trial_never_rearms_on_a_later_boot() {
        let (_directory, paths) = fixture("device\n");
        UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()))
            .schedule_host_trial_next_boot(Duration::from_secs(300))
            .expect("schedule");
        let mut command = FakeCommand::new(paths.usb_role.clone());
        command.fail_timed_host = true;
        let manager = UsbRoleManager::new(paths.clone(), command);
        let transaction = manager.begin_transaction().expect("boot transaction");
        let timeout = transaction
            .consume_host_trial_next_boot()
            .expect("consume")
            .expect("scheduled trial");

        assert!(transaction.host_trial(timeout).is_err());
        assert!(!paths.next_boot_host_trial.exists());
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert!(transaction.recovery_required());
    }

    #[test]
    fn next_boot_marker_write_fault_leaves_device_mode_unchanged() {
        let (_directory, paths) = fixture("device\n");
        fs::create_dir_all(&paths.state_dir).expect("state directory");
        let temporary = paths
            .state_dir
            .join(format!(".host-trial-next-boot.{}.tmp", std::process::id()));
        fs::create_dir(&temporary).expect("blocking temporary path");
        let manager = UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()));

        assert!(matches!(
            manager.schedule_host_trial_next_boot(Duration::from_secs(300)),
            Err(UsbRoleError::Io { .. })
        ));
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert!(manager.command.calls.borrow().is_empty());
        assert!(!paths.recovery_required.exists());
    }

    #[test]
    fn host_boot_defers_to_consumer_without_destroying_scheduled_trial() {
        let (_directory, paths) = fixture("device\n");
        let manager = UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()));
        manager
            .schedule_host_trial_next_boot(Duration::from_secs(300))
            .expect("schedule");

        assert_eq!(
            manager.host_boot().expect("host boot safeguard"),
            HostBootOutcome::TimedTrial
        );
        assert!(paths.next_boot_host_trial.exists());
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert!(manager.command.calls.borrow().is_empty());
    }

    #[test]
    fn pending_recovery_wins_over_a_stale_next_boot_trial() {
        let (_directory, paths) = fixture("device\n");
        let manager = UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()));
        manager
            .schedule_host_trial_next_boot(Duration::from_secs(300))
            .expect("schedule");
        create_state(&paths.recovery_required, b"required\n").expect("recovery marker");

        assert_eq!(
            manager.host_boot().expect("fail-safe boot"),
            HostBootOutcome::NotConfirmed
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert!(!paths.next_boot_host_trial.exists());
        assert!(!paths.recovery_required.exists());
        assert_eq!(
            manager.command.calls.borrow().last(),
            Some(&vec!["to-device".to_owned()])
        );
    }

    #[test]
    fn explicit_device_mode_cancels_a_scheduled_next_boot_trial() {
        let (_directory, paths) = fixture("device\n");
        let manager = UsbRoleManager::new(paths.clone(), FakeCommand::new(paths.usb_role.clone()));
        manager
            .schedule_host_trial_next_boot(Duration::from_secs(300))
            .expect("schedule");
        manager.device_mode().expect("device mode");
        assert!(!paths.next_boot_host_trial.exists());
    }

    #[test]
    fn confirm_requires_host_role() {
        let (_directory, paths) = fixture("device\n");
        let marker = paths.persistent_host.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert!(matches!(
            manager.confirm_host(),
            Err(UsbRoleError::RoleMismatch { .. })
        ));
        assert!(!marker.exists());
    }

    #[test]
    fn confirmation_cannot_convert_a_scheduled_trial_into_persistent_host() {
        let (_directory, paths) = fixture("host\n");
        create_state(
            &paths.next_boot_host_trial,
            &encode_next_boot_host_trial(Duration::from_secs(300)),
        )
        .expect("scheduled marker");
        let marker = paths.persistent_host.clone();
        let command = FakeCommand::new(paths.usb_role.clone());
        command.revert_scheduled.set(true);
        let manager = UsbRoleManager::new(paths, command);

        assert!(matches!(
            manager.confirm_host(),
            Err(UsbRoleError::NextBootHostTrialPending)
        ));
        assert!(!marker.exists());
        assert!(manager.command.calls.borrow().is_empty());
    }

    #[test]
    fn trial_refuses_an_existing_confirmation() {
        let (_directory, paths) = fixture("host\n");
        create_state(&paths.persistent_host, b"confirmed\n").expect("confirmation");
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert!(matches!(
            manager.host_trial(Duration::from_secs(300)),
            Err(UsbRoleError::HostAlreadyConfirmed)
        ));
        assert!(manager.command.calls.borrow().is_empty());
    }

    #[test]
    fn trial_refuses_a_preexisting_recovery_request() {
        let (_directory, paths) = fixture("device\n");
        create_state(&paths.recovery_required, b"required\n").expect("pending recovery");
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert!(matches!(
            manager.host_trial(Duration::from_secs(300)),
            Err(UsbRoleError::RecoveryPending)
        ));
        assert!(manager.command.calls.borrow().is_empty());
        assert_eq!(
            fs::read(&manager.paths.recovery_required).expect("recovery marker retained"),
            b"required\n"
        );
    }

    #[test]
    fn failed_device_transition_preserves_retry_state() {
        let (_directory, paths) = fixture("host\n");
        create_state(&paths.persistent_host, b"confirmed\n").expect("confirmation");
        let marker = paths.persistent_host.clone();
        let recovery = paths.recovery_required.clone();
        let mut command = FakeCommand::new(paths.usb_role.clone());
        command.fail_to_device = true;
        let manager = UsbRoleManager::new(paths, command);

        assert!(manager.device_mode().is_err());
        assert_eq!(
            fs::read(marker).expect("confirmation retained"),
            b"confirmed\n"
        );
        assert_eq!(
            fs::read(recovery).expect("recovery retained"),
            b"required\n"
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Host);
    }

    #[test]
    fn global_lifecycle_lock_serializes_and_releases_with_owner() {
        let directory = TempDir::new().expect("temporary directory");
        let path = directory.path().join("role.lock");
        let first = OperationLock::acquire_with_timeout(&path, Duration::from_millis(30))
            .expect("first owner");

        assert!(matches!(
            OperationLock::acquire_with_timeout(&path, Duration::from_millis(30)),
            Err(UsbRoleError::LockTimeout { .. })
        ));
        drop(first);
        OperationLock::acquire_with_timeout(&path, Duration::from_millis(30))
            .expect("lock released");
    }

    #[test]
    fn subprocess_deadline_kills_background_descendants_holding_output() {
        let directory = TempDir::new().expect("temporary directory");
        let sentinel = directory.path().join("descendant-survived");
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "(sleep 0.4; printf survived > \"$1\") &",
            "prelude-timeout-test",
            sentinel.to_str().expect("UTF-8 fixture path"),
        ]);
        let started = Instant::now();

        assert!(matches!(
            process_output_with_timeout(
                &mut command,
                Path::new("/bin/sh"),
                Duration::from_millis(50)
            ),
            Err(UsbRoleError::CommandTimeout { .. })
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        thread::sleep(Duration::from_millis(600));
        assert!(!sentinel.exists());
    }

    #[test]
    fn failed_timer_creation_rolls_back_to_device() {
        let (_directory, paths) = fixture("device\n");
        let mut command = FakeCommand::new(paths.usb_role.clone());
        command.fail_timed_host = true;
        let manager = UsbRoleManager::new(paths, command);

        assert!(manager.host_trial(Duration::from_secs(300)).is_err());
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert_eq!(
            manager.command.calls.borrow().last(),
            Some(&vec!["to-device".to_owned()])
        );
    }

    #[test]
    fn missing_timer_after_host_transition_rolls_back() {
        let (_directory, paths) = fixture("device\n");
        let mut command = FakeCommand::new(paths.usb_role.clone());
        command.schedule_after_host = false;
        let manager = UsbRoleManager::new(paths, command);

        assert!(matches!(
            manager.host_trial(Duration::from_secs(300)),
            Err(UsbRoleError::RevertNotScheduled)
        ));
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
    }

    #[test]
    fn confirm_requires_live_trial_and_cancelled_timer() {
        let (_directory, paths) = fixture("host\n");
        let marker = paths.persistent_host.clone();
        let mut command = FakeCommand::new(paths.usb_role.clone());
        command.revert_scheduled.set(true);
        command.cancel_works = false;
        let manager = UsbRoleManager::new(paths, command);

        assert!(matches!(
            manager.confirm_host(),
            Err(UsbRoleError::RevertStillScheduled)
        ));
        assert!(!marker.exists());
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
    }

    #[test]
    fn confirmation_marker_failure_rolls_back_to_device() {
        let (_directory, paths) = fixture("host\n");
        fs::create_dir_all(&paths.persistent_host).expect("blocking marker directory");
        let command = FakeCommand::new(paths.usb_role.clone());
        command.revert_scheduled.set(true);
        let manager = UsbRoleManager::new(paths, command);

        assert!(manager.confirm_host().is_err());
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
        assert!(!manager.persistent_host_confirmed());
    }

    #[test]
    fn corrupt_marker_is_not_confirmation() {
        let (_directory, paths) = fixture("device\n");
        fs::create_dir_all(&paths.state_dir).expect("state directory");
        fs::write(&paths.persistent_host, b"").expect("corrupt marker");
        let command = FakeCommand::new(paths.usb_role.clone());
        let manager = UsbRoleManager::new(paths, command);

        assert!(!manager.persistent_host_confirmed());
        assert_eq!(
            manager.host_boot().expect("safe boot"),
            HostBootOutcome::NotConfirmed
        );
        assert_eq!(manager.current().expect("role"), UsbRole::Device);
    }
}
