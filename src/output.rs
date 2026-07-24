//! Append-only JSONL writer + Alert struct.

use anyhow::{anyhow, Result};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

pub struct Writer {
    f: File,
}

pub fn open(path: &str) -> Result<Writer> {
    if let Some(dir) = Path::new(path).parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| anyhow!("mkdir {}: {}", dir.display(), e))?;
        }
    }
    let f = OpenOptions::new().append(true).create(true).open(path)?;
    Ok(Writer { f })
}

impl Writer {
    /// Serialises v as a single JSONL line.
    pub fn write_json<T: Serialize>(&mut self, v: &T) -> Result<()> {
        let mut b = serde_json::to_vec(v)?;
        b.push(b'\n');
        self.f.write_all(&b)?;
        Ok(())
    }
}

#[derive(Serialize)]
pub struct Alert {
    /// always "alert"
    pub event: String,
    pub kind: String,
    pub scraped_at: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub username: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub saw_keys: Vec<String>,
}

pub fn new_alert(kind: &str) -> Alert {
    Alert {
        event: "alert".to_string(),
        kind: kind.to_string(),
        scraped_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        username: String::new(),
        note: String::new(),
        missing: Vec::new(),
        saw_keys: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alert_omits_empty_fields() {
        let a = new_alert("logged_out");
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(v["event"], "alert");
        assert_eq!(v["kind"], "logged_out");
        assert!(v.get("username").is_none());
        assert!(v.get("note").is_none());
        assert!(v.get("missing").is_none());
        assert!(v.get("saw_keys").is_none());
        // RFC3339, seconds precision, Z suffix -- RFC3339.
        let ts = v["scraped_at"].as_str().unwrap();
        assert!(ts.ends_with('Z') && ts.len() == 20, "bad ts {ts}");
    }

    #[test]
    fn writer_appends_lines() {
        let dir = std::env::temp_dir().join("instagrab-output-test");
        let _ = fs::remove_dir_all(&dir);
        let p = dir.join("nested").join("runs.jsonl");
        let path = p.to_str().unwrap();
        {
            let mut w = open(path).unwrap();
            w.write_json(&serde_json::json!({"a": 1})).unwrap();
        }
        {
            let mut w = open(path).unwrap();
            w.write_json(&serde_json::json!({"b": 2})).unwrap();
        }
        let content = fs::read_to_string(path).unwrap();
        assert_eq!(content, "{\"a\":1}\n{\"b\":2}\n");
    }
}
