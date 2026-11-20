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

use std::ffi::OsString;
use std::process::{Command, Output};

use crate::core::registry::StepError;

/// Environment variable that names the adb binary. Default is `adb`.
pub const ADB_ENV: &str = "DA_ADB";

/// Environment variable that names the device to drive.
pub const SERIAL_ENV: &str = "DA_ANDROID_SERIAL";

/// The message for a run with no device attached: it names the next action.
const NO_DEVICE: &str = "no Android device is attached: connect one over USB or start an emulator, then check `adb devices`";

/// One device command line: which adb binary, and which device.
#[derive(Debug, Clone)]
pub(crate) struct Adb {
    program: OsString,
    /// Whether `DA_ADB` named the program, which decides what a failed start says.
    configured: bool,
    serial: Option<String>,
    /// Whether the step named the serial, which decides what a missing one says.
    serial_from_step: bool,
}

impl Adb {
    /// Reads the binary from `DA_ADB` and the device from `serial` or
    /// `DA_ANDROID_SERIAL`.
    ///
    /// Nothing is probed here: a step that never runs a command, for example
    /// because its arguments are invalid, must not need a device.
    pub(crate) fn from_arguments(serial: Option<&str>) -> Self {
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

    fn invoke(&self, argv: &[&str]) -> Result<Output, StepError> {
        self.command()
            .args(argv)
            .output()
            .map_err(|error| self.spawn_error(&error))
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
    /// nothing, the command's own output is reported instead of hiding it.
    fn failure(&self, action: &str, output: &Output) -> StepError {
        if let Some(problem) = self.device_problem() {
            return problem;
        }
        let detail = excerpt(output);
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
fn excerpt(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let text = if stderr.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
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
}
