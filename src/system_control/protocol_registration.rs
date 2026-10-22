//! Per-user protocol registration requires an explicit caller approval.
use anyhow::{Result, ensure};
use std::{
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
    System::Registry::*,
};

const ROOT: &str = r"Software\Classes\lxmusic";
const MAX_VALUE_BYTES: u32 = 64 * 1024;
struct RegistryKey(HKEY);
// Registry key handles have no thread affinity. This wrapper has one close
// owner; mutation methods require exclusive receipt access.
unsafe impl Send for RegistryKey {}
unsafe impl Sync for RegistryKey {}
impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn open(path: &str, write: bool, create: bool) -> Result<RegistryKey> {
    let path = wide(path);
    let mut key = ptr::null_mut();
    let status = unsafe {
        if create {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                ptr::null(),
                &mut key,
                ptr::null_mut(),
            )
        } else {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                if write {
                    KEY_QUERY_VALUE | KEY_SET_VALUE
                } else {
                    KEY_QUERY_VALUE
                },
                &mut key,
            )
        }
    };
    ensure!(
        status == ERROR_SUCCESS,
        "Cannot access per-user LX protocol key: {status}"
    );
    Ok(RegistryKey(key))
}
fn open_existing(path: &str) -> Result<Option<RegistryKey>> {
    let path = wide(path);
    let mut key = ptr::null_mut();
    let result = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            path.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    ensure!(
        result == ERROR_SUCCESS,
        "Cannot inspect existing per-user LX protocol ownership: {result}"
    );
    Ok(Some(RegistryKey(key)))
}
fn open_command(root: &RegistryKey) -> Result<RegistryKey> {
    let path = wide(r"shell\open\command");
    let mut key = ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            root.0,
            path.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    ensure!(
        status == ERROR_SUCCESS,
        "Cannot open owned LX protocol command: {status}"
    );
    Ok(RegistryKey(key))
}
fn read(key: &RegistryKey, name: &str) -> Result<Option<String>> {
    let name = wide(name);
    let mut kind = 0;
    let mut bytes = 0;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null(),
            &mut kind,
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    ensure!(
        result == ERROR_SUCCESS && kind == REG_SZ && bytes <= MAX_VALUE_BYTES && bytes % 2 == 0,
        "LX protocol value is not a bounded string"
    );
    let mut data = vec![0u16; bytes as usize / 2];
    ensure!(
        unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                data.as_mut_ptr().cast(),
                &mut bytes,
            )
        } == ERROR_SUCCESS
            && kind == REG_SZ,
        "LX protocol value changed while reading"
    );
    while data.last() == Some(&0) {
        data.pop();
    }
    Ok(Some(String::from_utf16(&data)?))
}
fn write(key: &RegistryKey, name: &str, value: &str) -> Result<()> {
    let name = wide(name);
    let value = wide(value);
    ensure!(
        unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                REG_SZ,
                value.as_ptr().cast(),
                u32::try_from(value.len() * 2)?,
            )
        } == ERROR_SUCCESS,
        "Cannot save per-user LX protocol value"
    );
    Ok(())
}
fn quote(value: &str) -> Result<String> {
    ensure!(
        !value.is_empty() && value.len() <= 16 * 1024 && !value.contains(['\0', '"', '\r', '\n']),
        "Protocol launcher argument is invalid"
    );
    Ok(format!("\"{value}\""))
}

pub struct ProtocolRegistrationApproval {
    executable: PathBuf,
    command: String,
}
impl ProtocolRegistrationApproval {
    /// This constructor represents the user explicitly enabling protocol
    /// integration. It is not deserializable and performs no registry mutation.
    pub fn for_explicit_enable(executable: &Path, arguments: &[String]) -> Result<Self> {
        ensure!(
            executable.is_absolute() && executable.is_file(),
            "Protocol launcher must be an actual absolute executable"
        );
        ensure!(
            arguments.len() <= 8,
            "Protocol launcher argument count exceeded"
        );
        let executable = std::fs::canonicalize(executable)?;
        let path = executable
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Protocol executable path is not Unicode"))?;
        let mut command = quote(path)?;
        for argument in arguments {
            command.push(' ');
            command.push_str(&quote(argument)?);
        }
        command.push_str(" \"%1\"");
        ensure!(
            command.encode_utf16().count() < MAX_VALUE_BYTES as usize / 2,
            "Protocol launcher command exceeds its registry readback budget"
        );
        Ok(Self {
            executable,
            command,
        })
    }
}
pub struct ProtocolRegistrationReceipt {
    executable: PathBuf,
    keys: RegistryValues,
    plan: ValuePlan,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolValueProgress {
    Unchanged,
    Installing,
    Installed,
    Removing,
    Removed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ProtocolRegistrationProgress {
    pub command: ProtocolValueProgress,
    pub name: ProtocolValueProgress,
    pub marker: ProtocolValueProgress,
}
pub struct ProtocolRegistrationFailure {
    pub error: anyhow::Error,
    pub recovery: Option<Box<ProtocolRegistrationReceipt>>,
}
impl std::fmt::Debug for ProtocolRegistrationReceipt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProtocolRegistrationReceipt")
            .field("executable", &self.executable)
            .field("progress", &self.progress())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for ProtocolRegistrationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProtocolRegistrationFailure")
            .field("error", &self.error)
            .field("recovery", &self.recovery)
            .finish()
    }
}
impl std::fmt::Display for ProtocolRegistrationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.error)
    }
}
impl std::error::Error for ProtocolRegistrationFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}
#[derive(Default)]
struct RegistryValues {
    command: Option<RegistryKey>,
    root: Option<RegistryKey>,
}
impl RegistryValues {
    fn prepare(&mut self) -> Result<()> {
        if self.root.is_none() {
            self.root = Some(open(ROOT, true, true)?);
        }
        if self.command.is_none() {
            self.command = Some(open_command(
                self.root.as_ref().expect("Owned registry root"),
            )?);
        }
        Ok(())
    }
    fn field(&self, index: usize) -> Result<(&RegistryKey, &'static str)> {
        match index {
            0 => Ok((
                self.command
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Protocol command owner is missing"))?,
                "",
            )),
            1 => Ok((
                self.root
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Protocol root owner is missing"))?,
                "",
            )),
            2 => Ok((
                self.root
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("Protocol root owner is missing"))?,
                "URL Protocol",
            )),
            _ => anyhow::bail!("Invalid protocol value index"),
        }
    }
}
trait ValueAccess {
    fn read(&mut self, index: usize) -> Result<Option<String>>;
    fn write(&mut self, index: usize, value: &str) -> Result<()>;
    fn remove(&mut self, index: usize) -> Result<()>;
}
impl ValueAccess for RegistryValues {
    fn read(&mut self, index: usize) -> Result<Option<String>> {
        let (key, name) = self.field(index)?;
        read(key, name)
    }
    fn write(&mut self, index: usize, value: &str) -> Result<()> {
        let (key, name) = self.field(index)?;
        write(key, name, value)
    }
    fn remove(&mut self, index: usize) -> Result<()> {
        let (key, name) = self.field(index)?;
        let status = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
        ensure!(
            status == ERROR_SUCCESS,
            "Cannot remove owned LX protocol value: {status}"
        );
        Ok(())
    }
}
struct ValuePlan {
    wanted: [String; 3],
    before: Option<[Option<String>; 3]>,
    progress: [ProtocolValueProgress; 3],
    retiring: bool,
}
impl ValuePlan {
    fn new(command: String) -> Self {
        Self {
            wanted: [
                command,
                "URL:LX Native music protocol".into(),
                String::new(),
            ],
            before: None,
            progress: [ProtocolValueProgress::Unchanged; 3],
            retiring: false,
        }
    }
    fn check(&self, values: &mut impl ValueAccess, retiring: bool) -> Result<()> {
        let before = self
            .before
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Protocol ownership has not been inspected"))?;
        for (index, old) in before.iter().enumerate() {
            let actual = values.read(index)?;
            let owned = actual.as_deref() == Some(self.wanted[index].as_str());
            let allowed = match self.progress[index] {
                ProtocolValueProgress::Unchanged => actual == *old,
                ProtocolValueProgress::Installing => {
                    actual == *old || owned || (retiring && actual.is_none())
                }
                ProtocolValueProgress::Installed => owned || (retiring && actual.is_none()),
                ProtocolValueProgress::Removing => owned || actual.is_none(),
                ProtocolValueProgress::Removed => actual.is_none(),
            };
            ensure!(
                allowed,
                "LX protocol value changed owner during its transaction"
            );
        }
        Ok(())
    }
    fn install(&mut self, values: &mut impl ValueAccess) -> Result<()> {
        ensure!(!self.retiring, "Protocol receipt is already retiring");
        if self.before.is_none() {
            let before = [values.read(0)?, values.read(1)?, values.read(2)?];
            ensure!(
                before
                    .iter()
                    .zip(&self.wanted)
                    .all(|(actual, wanted)| actual.as_ref().is_none_or(|actual| actual == wanted)),
                "LX protocol is registered to another program or has foreign metadata"
            );
            self.before = Some(before);
        }
        for index in 0..3 {
            self.check(values, false)?;
            if self.progress[index] == ProtocolValueProgress::Installed {
                continue;
            }
            self.progress[index] = ProtocolValueProgress::Installing;
            values.write(index, &self.wanted[index])?;
            ensure!(
                values.read(index)?.as_deref() == Some(self.wanted[index].as_str()),
                "Protocol value write was not confirmed"
            );
            self.progress[index] = ProtocolValueProgress::Installed;
        }
        self.check(values, false)
    }
    fn uninstall(&mut self, values: &mut impl ValueAccess) -> Result<()> {
        self.retiring = true;
        if self.before.is_none() {
            return Ok(());
        }
        for index in [0, 2, 1] {
            self.check(values, true)?;
            if matches!(
                self.progress[index],
                ProtocolValueProgress::Unchanged | ProtocolValueProgress::Removed
            ) {
                continue;
            }
            if values.read(index)?.is_none() {
                self.progress[index] = ProtocolValueProgress::Removed;
                continue;
            }
            ensure!(
                values.read(index)?.as_deref() == Some(self.wanted[index].as_str()),
                "LX protocol value changed owner before removal"
            );
            self.progress[index] = ProtocolValueProgress::Removing;
            values.remove(index)?;
            ensure!(
                values.read(index)?.is_none(),
                "Protocol value removal was not confirmed"
            );
            self.progress[index] = ProtocolValueProgress::Removed;
        }
        self.check(values, true)
    }
}
impl ProtocolRegistrationReceipt {
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    pub fn progress(&self) -> ProtocolRegistrationProgress {
        ProtocolRegistrationProgress {
            command: self.plan.progress[0],
            name: self.plan.progress[1],
            marker: self.plan.progress[2],
        }
    }
    /// Resumes the same approved registration with retained actual key handles.
    pub fn retry_registration(mut self) -> std::result::Result<Self, ProtocolRegistrationFailure> {
        let result = self
            .keys
            .prepare()
            .and_then(|()| self.plan.install(&mut self.keys));
        match result {
            Ok(()) => Ok(self),
            Err(error) => Err(ProtocolRegistrationFailure {
                error,
                recovery: Some(Box::new(self)),
            }),
        }
    }
    /// Removes only unchanged values owned by this registration. A foreign
    /// handler or user-edited command remains untouched.
    pub fn unregister(mut self) -> std::result::Result<(), ProtocolRegistrationFailure> {
        match self.plan.uninstall(&mut self.keys) {
            Ok(()) => Ok(()),
            Err(error) => Err(ProtocolRegistrationFailure {
                error,
                recovery: Some(Box::new(self)),
            }),
        }
    }
}
pub fn register_lxmusic_protocol(
    approval: ProtocolRegistrationApproval,
) -> std::result::Result<ProtocolRegistrationReceipt, ProtocolRegistrationFailure> {
    let preflight = (|| -> Result<()> {
        if let Some(existing) = open_existing(&format!(r"{ROOT}\shell\open\command"))? {
            ensure!(
                read(&existing, "")?.is_none_or(|value| value == approval.command),
                "LX protocol is registered to another program"
            );
        }
        if let Some(existing) = open_existing(ROOT)? {
            ensure!(
                read(&existing, "")?.is_none_or(|value| value == "URL:LX Native music protocol")
                    && read(&existing, "URL Protocol")?.is_none_or(|value| value.is_empty()),
                "LX protocol root has foreign metadata"
            );
        }
        Ok(())
    })();
    preflight.map_err(|error| ProtocolRegistrationFailure {
        error,
        recovery: None,
    })?;
    ProtocolRegistrationReceipt {
        plan: ValuePlan::new(approval.command),
        executable: approval.executable,
        keys: RegistryValues::default(),
    }
    .retry_registration()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Operation {
        Write,
        Remove,
    }
    #[derive(Default)]
    struct MemoryValues {
        fields: [Option<String>; 3],
        calls: Vec<(Operation, usize)>,
        fail: Option<(Operation, usize, bool)>,
    }
    impl MemoryValues {
        fn mutation(
            &mut self,
            operation: Operation,
            index: usize,
            value: Option<String>,
        ) -> Result<()> {
            self.calls.push((operation, index));
            let fault = self
                .fail
                .is_some_and(|(kind, field, _)| kind == operation && field == index);
            if fault && !self.fail.unwrap().2 {
                self.fail = None;
                anyhow::bail!("Injected failure before the actual value effect");
            }
            self.fields[index] = value;
            if fault {
                self.fail = None;
                anyhow::bail!("Injected failure after the actual value effect");
            }
            Ok(())
        }
    }
    impl ValueAccess for MemoryValues {
        fn read(&mut self, index: usize) -> Result<Option<String>> {
            Ok(self.fields[index].clone())
        }
        fn write(&mut self, index: usize, value: &str) -> Result<()> {
            self.mutation(Operation::Write, index, Some(value.into()))
        }
        fn remove(&mut self, index: usize) -> Result<()> {
            self.mutation(Operation::Remove, index, None)
        }
    }

    fn plan() -> ValuePlan {
        ValuePlan::new(r#""C:\selected\lx-native.exe" "%1""#.into())
    }

    #[test]
    fn every_failed_value_install_retains_attempt_and_resumes_its_same_plan() {
        for field in 0..3 {
            for after in [false, true] {
                let mut plan = plan();
                let mut values = MemoryValues {
                    fail: Some((Operation::Write, field, after)),
                    ..Default::default()
                };
                assert!(plan.install(&mut values).is_err());
                assert_eq!(plan.progress[field], ProtocolValueProgress::Installing);
                assert_eq!(
                    values.fields[field].as_ref(),
                    after.then_some(&plan.wanted[field])
                );
                assert!(plan.before.is_some());
                plan.install(&mut values).unwrap();
                assert_eq!(plan.progress, [ProtocolValueProgress::Installed; 3]);
                assert_eq!(values.fields, plan.wanted.clone().map(Some));
                plan.uninstall(&mut values).unwrap();
                assert_eq!(plan.progress, [ProtocolValueProgress::Removed; 3]);
                assert_eq!(values.fields, [None, None, None]);
            }
        }
    }

    #[test]
    fn every_failed_removal_retains_prior_progress_without_redeleting_completed_values() {
        for field in 0..3 {
            for after in [false, true] {
                let mut plan = plan();
                let mut values = MemoryValues::default();
                plan.install(&mut values).unwrap();
                values.calls.clear();
                values.fail = Some((Operation::Remove, field, after));
                assert!(plan.uninstall(&mut values).is_err());
                assert_eq!(plan.progress[field], ProtocolValueProgress::Removing);
                assert_eq!(
                    values.fields[field].as_ref(),
                    (!after).then_some(&plan.wanted[field])
                );
                let completed = plan
                    .progress
                    .map(|progress| progress == ProtocolValueProgress::Removed);
                let old_calls = values.calls.clone();
                plan.uninstall(&mut values).unwrap();
                assert_eq!(values.fields, [None, None, None]);
                for (index, was_complete) in completed.iter().enumerate() {
                    if *was_complete {
                        assert_eq!(
                            values
                                .calls
                                .iter()
                                .filter(|call| **call == (Operation::Remove, index))
                                .count(),
                            old_calls
                                .iter()
                                .filter(|call| **call == (Operation::Remove, index))
                                .count()
                        );
                    }
                }
                assert!(plan.install(&mut values).is_err());
            }
        }
    }

    #[test]
    fn a_foreign_handler_is_never_overwritten_or_removed_after_partial_ownership() {
        let mut fresh = plan();
        let mut foreign = MemoryValues {
            fields: [Some("Foreign protocol handler".into()), None, None],
            ..Default::default()
        };
        assert!(fresh.install(&mut foreign).is_err());
        assert!(foreign.calls.is_empty());
        assert!(fresh.before.is_none());

        let mut partial = plan();
        let mut values = MemoryValues::default();
        partial.install(&mut values).unwrap();
        values.fail = Some((Operation::Remove, 2, false));
        assert!(partial.uninstall(&mut values).is_err());
        assert_eq!(partial.progress[0], ProtocolValueProgress::Removed);
        values.fields[0] = Some("Foreign protocol handler".into());
        let before = values.fields.clone();
        let calls = values.calls.len();
        assert!(partial.uninstall(&mut values).is_err());
        assert_eq!(values.fields, before);
        assert_eq!(values.calls.len(), calls);
    }

    #[test]
    fn failed_install_can_retire_only_values_that_its_actual_attempt_owned() {
        let mut plan = plan();
        let mut values = MemoryValues {
            fail: Some((Operation::Write, 1, true)),
            ..Default::default()
        };
        assert!(plan.install(&mut values).is_err());
        assert_eq!(plan.progress[0], ProtocolValueProgress::Installed);
        assert_eq!(plan.progress[1], ProtocolValueProgress::Installing);
        assert_eq!(plan.progress[2], ProtocolValueProgress::Unchanged);
        values.calls.clear();
        plan.uninstall(&mut values).unwrap();
        assert_eq!(values.fields, [None, None, None]);
        assert_eq!(
            values.calls,
            [(Operation::Remove, 0), (Operation::Remove, 1)]
        );
    }

    #[test]
    fn typed_failure_can_retain_actual_key_owners_across_caller_threads() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<ProtocolRegistrationFailure>();
    }
}
