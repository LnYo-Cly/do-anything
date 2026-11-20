//! How a step reaches a device: one adb binary and one device serial.
//!
//! Every device call is one `adb` process. The binary owns the server, the USB
//! or network transport, the debugging authorization and device selection, so a
//! driver that spawns it inherits all of that without speaking the wire
//! protocol.
//!
//! The binary comes from `DA_ADB` and defaults to `adb`; the device comes from
//! the step's `serial` or from `DA_ANDROID_SERIAL`. A command that fails is
//! explained with the next action the user can take, and the explanation never
//! repeats the configured serial or path: a run's output is written to reports
//! and failure bundles, and a serial identifies one physical device.
//!
//! Every command has a deadline. A transport that drops its connection, a
//! `uiautomator dump` that never returns or a wedged input method all leave adb
//! waiting without an answer, and none of them ends on its own, so without a
//! deadline the run hangs until CI or the operator kills it. When the deadline
//! passes the `adb` process is killed and the command fails with the step, the
//! deadline, the time that passed and the command it ran. Killing adb is safe:
//! it keeps no state between calls, so the next call starts from the device as
//! it is.
//!
//! A test replaces the binary with a script through `DA_ADB`, so the deadline
//! and the kill are proven without a device; `tests/android.rs` does that.

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::core::registry::StepError;

/// Environment variable that names the adb binary. Default is `adb`.
pub const ADB_ENV: &str = "DA_ADB";

/// Environment variable that names the device to drive.
pub const SERIAL_ENV: &str = "DA_ANDROID_SERIAL";

/// How long one adb command may run when the step asks for no deadline.
///
/// A dump costs about two seconds on the measured container and a tap 24 ms, so
/// thirty seconds leaves ample room for a slow image while still failing fast
/// on a device that stopped answering.
pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The longest deadline a step may ask for.
///
/// The deadline exists to turn a hung device into a failure, so a value that
/// outlives the run it belongs to is a mistake rather than a slow device: ten
/// minutes is already two hundred dumps, and a case that needs longer needs a
/// different device instead of a longer deadline.
pub(crate) const MAX_TIMEOUT_MS: u64 = 600_000;

/// How often a running command is checked against its deadline.
///
/// The wait is a poll rather than the `wait-timeout` crate, because the
/// `android` feature must build without the `cli.run` step and the dependency
/// that step enables. Ten milliseconds bound how late a command can be killed,
/// which is nothing against a deadline measured in seconds.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How long the output pipes may stay open after the command ended or died.
///
/// A process the command started keeps the write end of a pipe open after adb
/// itself is gone. Waiting for it without a bound would hang `da`, so the output
/// read so far is what a killed command reports.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

/// The message for a run with no device attached: it names the next action.
const NO_DEVICE: &str = "no Android device is attached: connect one over USB or start an emulator, then check `adb devices`";

/// One device command line: which adb binary, which device and how long one
/// command may run.
#[derive(Debug, Clone)]
pub(crate) struct Adb {
    program: OsString,
    /// Whether `DA_ADB` named the program, which decides what a failed start says.
    configured: bool,
    serial: Option<String>,
    /// Whether the step named the serial, which decides what a missing one says.
    serial_from_step: bool,
    /// The step this command line serves, named when a command times out.
    step: &'static str,
    /// How long one command may run before its process is killed.
    timeout: Duration,
}

impl Adb {
    /// Reads the binary from `DA_ADB`, the device from `serial` or
    /// `DA_ANDROID_SERIAL`, and takes the deadline from the step.
    ///
    /// Nothing is probed here: a step that never runs a command, for example
    /// because its arguments are invalid, must not need a device.
    pub(crate) fn from_arguments(
        step: &'static str,
        serial: Option<&str>,
        timeout_ms: u64,
    ) -> Self {
        let configured = std::env::var_os(ADB_ENV).filter(|value| !value.is_empty());
        let program = configured.clone().unwrap_or_else(|| OsString::from("adb"));
        let (serial, serial_from_step) = match serial {
            Some(value) => (Some(value.trim().to_string()), true),
            None => (
                std::env::var(SERIAL_ENV)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                false,
            ),
        };
        Self {
            configured: configured.is_some(),
            program,
            serial,
            serial_from_step,
            step,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// Runs one device command and returns its standard output as text.
    ///
    /// `action` names the work in the error message, for example "tap (10, 20)".
    pub(crate) fn shell(&self, argv: &[&str], action: &str) -> Result<String, StepError> {
        let mut full = vec!["shell"];
        full.extend_from_slice(argv);
        let output = self.run(&full, action)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Runs `adb exec-out`, which hands back the device's bytes unchanged.
    pub(crate) fn exec_out(&self, argv: &[&str], action: &str) -> Result<Vec<u8>, StepError> {
        let mut full = vec!["exec-out"];
        full.extend_from_slice(argv);
        self.run(&full, action).map(|output| output.stdout)
    }

    /// Runs one command, and explains it when it fails.
    fn run(&self, argv: &[&str], action: &str) -> Result<Output, StepError> {
        let output = self.invoke(argv)?;
        if output.status.success() {
            return Ok(output);
        }
        Err(self.failure(action, &output))
    }

    /// The command line, with the serial when one is configured.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        if let Some(serial) = &self.serial {
            command.arg("-s").arg(serial);
        }
        command
    }

    /// Runs one command under its deadline and returns everything it printed.
    ///
    /// The output is read on two threads while this thread waits for the
    /// process: a command that prints more than a pipe holds — a screenshot is
    /// a few hundred kilobytes — would otherwise block on its own output and
    /// never exit, which would turn a healthy command into a timeout.
    fn invoke(&self, argv: &[&str]) -> Result<Output, StepError> {
        let started = Instant::now();
        let mut child = self
            .command()
            .args(argv)
            // adb reads its standard input only to forward it to the device,
            // and a case has nothing to forward; inheriting the terminal's
            // input would let a command wait for it.
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| self.spawn_error(&error))?;
        let stdout = child.stdout.take().map(PipeReader::spawn);
        let stderr = child.stderr.take().map(PipeReader::spawn);

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    // The wait itself failed, so the command is still running
                    // and must not outlive the step.
                    kill(&mut child);
                    return Err(StepError::Environment(format!(
                        "could not wait for the adb command `{}`: {error}; check that the device answers `adb devices`",
                        self.command_line(argv)
                    )));
                }
            }
            if started.elapsed() >= self.timeout {
                let elapsed = started.elapsed();
                kill(&mut child);
                // Read what the command printed before it died: at a deadline
                // the output is what tells a slow device from a gone one.
                let (stdout, stderr) = drain(stdout, stderr);
                return Err(self.deadline_error(argv, elapsed, &stdout, &stderr));
            }
            thread::sleep(POLL_INTERVAL);
        };

        let (stdout, stderr) = drain(stdout, stderr);
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    /// The error for a command that outlived its deadline and was killed.
    ///
    /// It names the step, the deadline, the time that passed and the command,
    /// so a reader can tell a device that is only slow from one that stopped
    /// answering. It names neither the adb binary nor the serial: a report and
    /// a failure bundle are shared, and a path says which machine ran the case
    /// while a serial says which device it drove.
    fn deadline_error(
        &self,
        argv: &[&str],
        elapsed: Duration,
        stdout: &[u8],
        stderr: &[u8],
    ) -> StepError {
        let detail = excerpt(stdout, stderr);
        let printed = if detail.is_empty() {
            String::new()
        } else {
            format!(" (adb printed: {detail})")
        };
        StepError::Timeout(format!(
            "`{}` did not finish within its {} ms deadline: the adb command `{}` was killed after {} ms{printed}; raise `timeout_ms` if the device is only slow, or check that the device answers `adb devices`",
            self.step,
            self.timeout.as_millis(),
            self.command_line(argv),
            elapsed.as_millis(),
        ))
    }

    /// The command line as a message shows it: `adb`, the arguments, no serial.
    fn command_line(&self, argv: &[&str]) -> String {
        let mut line = String::from("adb");
        for argument in argv {
            line.push(' ');
            line.push_str(argument);
        }
        line
    }

    /// The same command line with a shorter deadline at most.
    ///
    /// Recording a failure is not the step's work, so its commands get the
    /// step's deadline or a shorter one the caller names: a device that has
    /// stopped answering must not add a second full deadline to a failure that
    /// already took one.
    pub(crate) fn bounded_by(&self, timeout: Duration) -> Self {
        let mut bounded = self.clone();
        bounded.timeout = bounded.timeout.min(timeout);
        bounded
    }

    /// The error for an adb binary that could not be started at all.
    fn spawn_error(&self, error: &std::io::Error) -> StepError {
        let message = if self.configured {
            format!(
                "could not run adb: `{ADB_ENV}` does not name a runnable adb binary ({error}); set `{ADB_ENV}` to the adb executable"
            )
        } else {
            format!(
                "could not run adb: `adb` could not be started ({error}); set `{ADB_ENV}` to the adb executable, or install the Android platform-tools and add them to `PATH`"
            )
        };
        StepError::Environment(message)
    }

    /// The error for a command the device or the transport refused.
    ///
    /// A failed command is asked why through the device list, because "no
    /// device", "more than one device" and a serial that names nothing are the
    /// three failures a case author causes and can fix. When the list explains
    /// nothing, the command's own output is reported instead of hiding it. When
    /// it does explain the failure, what adb printed is kept beside it: a device
    /// that dropped its transport mid-run says so in those words, and the list
    /// only says which state the device is in now.
    fn failure(&self, action: &str, output: &Output) -> StepError {
        let detail = excerpt(&output.stdout, &output.stderr);
        if let Some(problem) = self.device_problem() {
            return match (problem, detail.is_empty()) {
                (StepError::Environment(message), false) => {
                    StepError::Environment(format!("{message} (adb printed: {detail})"))
                }
                (problem, _) => problem,
            };
        }
        if detail.is_empty() {
            StepError::Execution(format!(
                "could not {action}: adb exited with {} and printed nothing",
                output.status
            ))
        } else {
            StepError::Execution(format!("could not {action}: {detail}"))
        }
    }

    /// Asks the device list why the command failed, when the list explains it.
    fn device_problem(&self) -> Option<StepError> {
        // A spawn failure belongs to the command that could not start, which
        // already reported it; this probe only classifies a list it could read.
        let output = self.invoke(&["devices"]).ok()?;
        if !output.status.success() {
            return None;
        }
        self.classify(&String::from_utf8_lossy(&output.stdout))
    }

    /// Turns the text of `adb devices` into the error that explains a failed
    /// command, or `None` when the device the command named is present and ready.
    fn classify(&self, devices: &str) -> Option<StepError> {
        let entries = parse_devices(devices);
        match select(&entries, self.serial.as_deref()) {
            Selection::Ready => None,
            Selection::NoDevice => Some(StepError::Environment(NO_DEVICE.to_string())),
            Selection::NotReady(state) => Some(StepError::Environment(format!(
                "the attached Android device is not ready (`{state}`): accept the USB debugging prompt on the device, then check `adb devices` again"
            ))),
            Selection::MissingSerial => Some(StepError::Environment(self.missing_serial_message())),
            Selection::Many(count) => Some(StepError::Environment(format!(
                "{count} Android devices are attached: set `{SERIAL_ENV}` to the serial of the one to drive, or pass `serial` to the step"
            ))),
        }
    }

    fn missing_serial_message(&self) -> String {
        if self.serial_from_step {
            "no attached Android device has the `serial` this step passes: check `adb devices` and pass one of the serials it lists"
                .to_string()
        } else {
            format!(
                "no attached Android device has the serial in `{SERIAL_ENV}`: check `adb devices` and set `{SERIAL_ENV}` to one of the serials it lists"
            )
        }
    }
}

/// Everything the two readers collected, each waited for at most
/// [`DRAIN_TIMEOUT`].
fn drain(stdout: Option<PipeReader>, stderr: Option<PipeReader>) -> (Vec<u8>, Vec<u8>) {
    (
        stdout.map(PipeReader::finish).unwrap_or_default(),
        stderr.map(PipeReader::finish).unwrap_or_default(),
    )
}

/// Kills one command's process and reaps it.
///
/// An unreaped child stays a zombie on Unix and holds its handle on Windows, so
/// the wait is part of the kill. Both calls are best effort: a process that
/// already exited fails the kill and still needs to be reaped.
fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads one output pipe on its own thread.
///
/// This is the reader `cli.run` uses, repeated here because the `android`
/// feature builds without the `cli` step and the crate its timeout needs; the
/// two drivers share nothing but the shape of the problem. The thread keeps the
/// child from blocking on a full pipe, and it is what still holds the output
/// when the child was killed.
struct PipeReader {
    buffer: Arc<Mutex<Vec<u8>>>,
    done: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl PipeReader {
    fn spawn<R: Read + Send + 'static>(mut pipe: R) -> Self {
        const CHUNK: usize = 8 * 1024;
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let shared = Arc::clone(&buffer);
        let (sender, done) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut chunk = [0_u8; CHUNK];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        if let Ok(mut buffer) = shared.lock() {
                            buffer.extend_from_slice(&chunk[..read]);
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            let _ = sender.send(());
        });
        Self {
            buffer,
            done,
            handle: Some(handle),
        }
    }

    /// Waits for the pipe to close, then returns everything that was read.
    fn finish(mut self) -> Vec<u8> {
        let closed = self.done.recv_timeout(DRAIN_TIMEOUT).is_ok();
        if closed && let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        // A reader that is still blocked stays detached with its pipe. It owns
        // nothing `da` needs, and the process ends without it.
        match self.buffer.lock() {
            Ok(buffer) => buffer.clone(),
            Err(_) => Vec::new(),
        }
    }
}

/// One device `adb devices` listed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceEntry {
    serial: String,
    state: String,
}

/// What the device list means for the device this command line names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selection {
    /// Exactly one device is ready to accept commands.
    Ready,
    /// No device is attached at all.
    NoDevice,
    /// A device is attached but not ready; its state.
    NotReady(&'static str),
    /// A serial was configured and no attached device has it.
    MissingSerial,
    /// More than one device is ready and no serial names one; the count.
    Many(usize),
}

/// Reads the lines of `adb devices`.
///
/// The header, the daemon start-up notes adb prints on the same stream and
/// anything that is not a `serial state` pair are left out, so a device list
/// from an older or newer adb classifies the same way.
fn parse_devices(text: &str) -> Vec<DeviceEntry> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('*')
            || line.starts_with("List of devices")
            || line.starts_with("adb server")
        {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(serial), Some(state)) = (parts.next(), parts.next()) else {
            continue;
        };
        entries.push(DeviceEntry {
            serial: serial.to_string(),
            state: state.to_string(),
        });
    }
    entries
}

/// Decides which device a command line drives.
///
/// A state other than `device` means the transport exists but the device is not
/// usable yet: `unauthorized` waits for the debugging prompt, `offline` for the
/// connection to settle, `no permissions` for a udev rule. Naming the state is
/// what tells the user which of those to fix.
fn select(entries: &[DeviceEntry], serial: Option<&str>) -> Selection {
    if let Some(serial) = serial {
        return match entries.iter().find(|entry| entry.serial == serial) {
            Some(entry) if entry.state == "device" => Selection::Ready,
            Some(entry) => Selection::NotReady(state_of(entry)),
            None => Selection::MissingSerial,
        };
    }
    let ready = entries
        .iter()
        .filter(|entry| entry.state == "device")
        .count();
    match ready {
        0 => match entries.first() {
            Some(entry) => Selection::NotReady(state_of(entry)),
            None => Selection::NoDevice,
        },
        1 => Selection::Ready,
        count => Selection::Many(count),
    }
}

/// The state of one entry, as the message shows it.
///
/// The states adb prints are a fixed set of ASCII words; anything else is
/// reported as `unknown` rather than copying an unbounded string into a
/// message.
fn state_of(entry: &DeviceEntry) -> &'static str {
    match entry.state.as_str() {
        "offline" => "offline",
        "unauthorized" => "unauthorized",
        "bootloader" => "bootloader",
        "recovery" => "recovery",
        "rescue" => "rescue",
        "no permissions" => "no permissions",
        _ => "unknown",
    }
}

/// The part of a command's output a person reads in an error message.
///
/// A device command may print help text or a whole hierarchy, so the first
/// lines are used instead of everything; `android.dump` reports its own result
/// and never relies on this.
fn excerpt(stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let text = if stderr.trim().is_empty() {
        String::from_utf8_lossy(stdout)
    } else {
        stderr
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adb(program: &str, serial: Option<&str>, from_step: bool) -> Adb {
        Adb {
            program: OsString::from(program),
            configured: false,
            serial: serial.map(str::to_string),
            serial_from_step: from_step,
            step: "android.tap",
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
        }
    }

    const TWO_DEVICES: &str =
        "List of devices attached\nemulator-5554\tdevice\n10.0.0.2:5555\tdevice\n";

    #[test]
    fn the_device_list_is_read_without_the_header_or_the_daemon_notes() {
        let entries = parse_devices(
            "List of devices attached\n* daemon not running; starting now at tcp:5037\n* daemon started successfully\nemulator-5554\tdevice\nabc123\toffline\r\n\n",
        );

        assert_eq!(
            entries,
            vec![
                DeviceEntry {
                    serial: "emulator-5554".to_string(),
                    state: "device".to_string(),
                },
                DeviceEntry {
                    serial: "abc123".to_string(),
                    state: "offline".to_string(),
                },
            ]
        );
    }

    #[test]
    fn an_empty_device_list_is_read_as_no_device() {
        assert!(parse_devices("").is_empty());
        assert!(parse_devices("List of devices attached\n").is_empty());
        assert_eq!(select(&[], None), Selection::NoDevice);
    }

    #[test]
    fn one_ready_device_is_selected_without_a_serial() {
        let one = parse_devices("List of devices attached\nemulator-5554\tdevice\n");
        assert_eq!(select(&one, None), Selection::Ready);
    }

    #[test]
    fn several_ready_devices_need_a_serial_to_choose_one() {
        let entries = parse_devices(TWO_DEVICES);
        assert_eq!(select(&entries, None), Selection::Many(2));
        assert_eq!(select(&entries, Some("10.0.0.2:5555")), Selection::Ready);
        assert_eq!(
            select(&entries, Some("another-serial")),
            Selection::MissingSerial
        );
    }

    #[test]
    fn a_device_that_is_not_ready_is_reported_with_its_state() {
        let entries = parse_devices("List of devices attached\nabc123\tunauthorized\n");
        assert_eq!(select(&entries, None), Selection::NotReady("unauthorized"));
        assert_eq!(
            select(&entries, Some("abc123")),
            Selection::NotReady("unauthorized")
        );
    }

    #[test]
    fn an_unknown_state_is_not_copied_into_the_message() {
        let entries = parse_devices("List of devices attached\nabc123\thost\n");
        assert_eq!(select(&entries, None), Selection::NotReady("unknown"));
    }

    #[test]
    fn the_command_line_names_the_serial_only_when_one_is_configured() {
        let named = adb("adb", Some("emulator-5554"), true).command();
        let args: Vec<String> = named
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["-s", "emulator-5554"]);

        let unnamed = adb("adb", None, false).command();
        assert_eq!(unnamed.get_args().count(), 0);
    }

    #[test]
    fn a_missing_adb_binary_is_an_environment_error_that_names_the_setting() {
        let error = adb("da-test-adb-binary-that-does-not-exist", None, false)
            .shell(&["devices"], "list the attached devices")
            .expect_err("a missing adb binary must not be a silent success");

        let StepError::Environment(message) = error else {
            panic!("a missing adb binary is an environment failure, got: {error:?}");
        };
        assert!(message.contains("could not run adb"), "got: {message}");
        assert!(message.contains(ADB_ENV), "got: {message}");
        assert!(message.contains("adb"), "got: {message}");
    }

    #[test]
    fn a_configured_adb_that_cannot_start_names_the_setting_it_came_from() {
        let mut configured = adb("da-test-adb-binary-that-does-not-exist", None, false);
        configured.configured = true;

        let error = configured
            .shell(&["devices"], "list the attached devices")
            .expect_err("a missing adb binary must not be a silent success");

        let StepError::Environment(message) = error else {
            panic!("a missing adb binary is an environment failure, got: {error:?}");
        };
        assert!(
            message.contains("`DA_ADB` does not name a runnable adb binary"),
            "got: {message}"
        );
    }

    #[test]
    fn the_device_list_explains_each_failure_with_the_next_action() {
        let none = adb("adb", None, false);
        let message = |error: StepError| match error {
            StepError::Environment(message) => message,
            other => panic!("a device problem is an environment failure, got: {other:?}"),
        };

        let no_device = message(
            none.classify("List of devices attached\n")
                .expect("no device must be explained"),
        );
        assert_eq!(
            no_device,
            "no Android device is attached: connect one over USB or start an emulator, then check `adb devices`"
        );

        let many = message(
            none.classify(TWO_DEVICES)
                .expect("two devices must be explained"),
        );
        assert!(
            many.starts_with("2 Android devices are attached"),
            "got: {many}"
        );
        assert!(many.contains(SERIAL_ENV), "got: {many}");
        assert!(many.contains("`serial`"), "got: {many}");

        let unauthorized = message(
            none.classify("List of devices attached\nabc123\tunauthorized\n")
                .expect("an unauthorized device must be explained"),
        );
        assert!(
            unauthorized.contains("`unauthorized`"),
            "got: {unauthorized}"
        );
        assert!(
            unauthorized.contains("USB debugging prompt"),
            "got: {unauthorized}"
        );
    }

    #[test]
    fn a_serial_that_matches_no_device_says_where_it_came_from() {
        let devices = "List of devices attached\nemulator-5554\tdevice\n";

        let from_env = adb("adb", Some("gone"), false);
        let StepError::Environment(message) = from_env
            .classify(devices)
            .expect("a missing serial must be explained")
        else {
            panic!("a missing serial is an environment failure");
        };
        assert!(
            message.contains(&format!("serial in `{SERIAL_ENV}`")),
            "got: {message}"
        );

        let from_step = adb("adb", Some("gone"), true);
        let StepError::Environment(message) = from_step
            .classify(devices)
            .expect("a missing serial must be explained")
        else {
            panic!("a missing serial is an environment failure");
        };
        assert!(
            message.contains("`serial` this step passes"),
            "got: {message}"
        );
        assert!(
            !message.contains("gone"),
            "the serial must not be repeated: {message}"
        );
    }

    #[test]
    fn a_ready_device_leaves_a_failed_command_unexplained() {
        let one = adb("adb", None, false);
        assert!(
            one.classify("List of devices attached\nemulator-5554\tdevice\n")
                .is_none(),
            "a ready device must let the command's own output be reported"
        );
    }

    #[test]
    fn a_deadline_names_the_step_the_deadline_the_time_and_the_command() {
        let mut deadlined = adb(
            "/opt/android-platform-tools/adb",
            Some("emulator-5554"),
            true,
        );
        deadlined.step = "android.dump";
        deadlined.timeout = Duration::from_millis(1_500);

        let StepError::Timeout(message) = deadlined.deadline_error(
            &["shell", "uiautomator", "dump"],
            Duration::from_millis(1_508),
            b"",
            b"",
        ) else {
            panic!("a missed deadline is a timeout");
        };

        assert!(message.contains("`android.dump`"), "got: {message}");
        assert!(message.contains("1500 ms"), "got: {message}");
        assert!(message.contains("after 1508 ms"), "got: {message}");
        assert!(
            message.contains("`adb shell uiautomator dump`"),
            "the command must be named: {message}"
        );
        assert!(message.contains("timeout_ms"), "got: {message}");
        assert!(
            !message.contains("android-platform-tools"),
            "the adb path must not leak: {message}"
        );
        assert!(
            !message.contains("emulator-5554"),
            "the serial must not leak: {message}"
        );
    }

    #[test]
    fn a_deadline_keeps_what_the_command_printed_before_it_was_killed() {
        let deadlined = adb("adb", None, false);

        let StepError::Timeout(message) = deadlined.deadline_error(
            &["shell", "input", "tap", "1", "2"],
            Duration::from_millis(30_005),
            b"",
            b"error: device offline\n",
        ) else {
            panic!("a missed deadline is a timeout");
        };

        assert!(
            message.contains("adb printed: error: device offline"),
            "the output the command did print must be kept: {message}"
        );
        assert!(
            message.contains("after 30005 ms"),
            "a command killed late must say how late: {message}"
        );
    }

    #[test]
    fn an_excerpt_prefers_what_the_command_wrote_to_standard_error() {
        assert_eq!(
            excerpt(b"", b"error: no devices/emulators found\n"),
            "error: no devices/emulators found"
        );
        assert_eq!(
            excerpt(b"List of devices attached\n", b"  \n"),
            "List of devices attached"
        );
        assert_eq!(
            excerpt(b"one\ntwo\nthree\nfour\n", b""),
            "one; two; three",
            "only the first lines belong in a message"
        );
        assert_eq!(excerpt(b"", b""), "");
    }
}
