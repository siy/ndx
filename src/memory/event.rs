use super::{EventEntry, MemoryIndex};
use anyhow::Result;
use std::io::{BufRead, Seek, SeekFrom};

pub struct EventScanResult {
    pub new_events: u64,
    pub total_events: u64,
}

pub fn ingest_events(memory: &MemoryIndex) -> Result<EventScanResult> {
    let home = dirs::home_dir().unwrap_or_default();
    let events_path = home.join(".kcp").join("events.jsonl");

    if !events_path.exists() {
        return Ok(EventScanResult {
            new_events: 0,
            total_events: 0,
        });
    }

    let cursor = memory.get_event_cursor()?;
    let file_len = std::fs::metadata(&events_path)?.len();

    // If file was truncated, reset cursor
    let seek_pos = if cursor > file_len { 0 } else { cursor };

    let mut file = std::fs::File::open(&events_path)?;
    file.seek(SeekFrom::Start(seek_pos))?;

    let reader = std::io::BufReader::new(&file);
    let mut new_events = 0u64;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let node: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_ts = node
            .get("ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = node
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let project_dir = node
            .get("project_dir")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let tool = node
            .get("tool")
            .and_then(|v| v.as_str())
            .unwrap_or("Bash")
            .to_string();
        let command = node
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let manifest_key = node
            .get("manifest_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let entry = EventEntry {
            event_ts,
            session_id,
            project_dir,
            tool,
            command: if command.len() > 500 {
                command[..500].to_string()
            } else {
                command
            },
            manifest_key,
            ingested_at: chrono::Utc::now().to_rfc3339(),
        };

        match memory.insert_event(&entry) {
            Ok(true) => new_events += 1,
            Ok(false) => {} // duplicate
            Err(e) => {
                tracing::warn!("failed to insert event: {}", e);
            }
        }
    }

    // Update cursor to end of file
    memory.set_event_cursor(file_len)?;

    Ok(EventScanResult {
        new_events,
        total_events: new_events, // approximate
    })
}
