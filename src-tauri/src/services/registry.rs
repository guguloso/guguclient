//! Minimal Windows registry access through `reg.exe`, mirroring the
//! Electron build's `queryRegistryValue`/`setRegistryValue`/
//! `deleteRegistryValue` helpers (which also shell out to `reg`).

use std::process::Command;

pub enum Query {
    Found { data: String, kind: String },
    Missing,
    Error(String),
}

fn reg() -> Command {
    let mut command = Command::new("reg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    command
}

/// `reg query <key> /v <name>` (or `/ve` for the default value).
pub fn query(key: &str, value_name: Option<&str>) -> Query {
    let mut command = reg();
    command.args(["query", key]);
    match value_name {
        Some(name) => command.args(["/v", name]),
        None => command.arg("/ve"),
    };
    let output = match command.output() {
        Ok(output) => output,
        Err(error) => return Query::Error(error.to_string()),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        return if stderr.contains("unable to find")
            || stderr.contains("não foi poss")
            || output.status.code() == Some(1)
        {
            Query::Missing
        } else {
            Query::Error(stderr.trim().to_owned())
        };
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let trimmed = line.trim_start();
        // `<name>    REG_SZ    <data>`; the default value's name is localized,
        // so for `/ve` take the first value line under the key.
        let matches_name = match value_name {
            Some(name) => {
                trimmed.starts_with(name) && trimmed[name.len()..].starts_with(char::is_whitespace)
            }
            None => trimmed.contains("REG_"),
        };
        if !matches_name || !line.starts_with(' ') {
            continue;
        }
        let Some(pos) = trimmed.find("REG_") else {
            continue;
        };
        let rest = &trimmed[pos..];
        let (kind, data) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        return Query::Found {
            data: data.trim().to_owned(),
            kind: kind.to_owned(),
        };
    }
    Query::Missing
}

pub fn set(key: &str, value_name: Option<&str>, kind: &str, data: &str) -> Result<(), String> {
    let mut command = reg();
    command.args(["add", key]);
    match value_name {
        Some(name) => command.args(["/v", name]),
        None => command.arg("/ve"),
    };
    command.args(["/t", kind, "/d", data, "/f"]);
    match command.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

/// Deletes one value (or the key's default value / the whole key when
/// `value_name` is `None` and the key has no other content).
pub fn delete(key: &str, value_name: Option<&str>) -> Result<(), String> {
    let mut command = reg();
    command.args(["delete", key]);
    if let Some(name) = value_name {
        command.args(["/v", name]);
    }
    command.arg("/f");
    match command.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_owned()),
        Err(error) => Err(error.to_string()),
    }
}
