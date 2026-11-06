//! Motion and near presence, from the sensors on the backplate: nlclient
//! keeps them to itself but logs each event in /media/log/nlclient, and
//! nestlocal follows that log.
//!
//! - "publishing pir msg true": the PIR, which sees motion across the room
//!   (the far sensor that shows the clock).
//! - "tp crossings": the near-field proximity sensor, which fires again and
//!   again while someone stands in front of the unit.
//!
//! Each event is published as ON; Home Assistant clears the sensor itself
//! (off_delay) after its hold without another, so a sensor clears on time
//! even while the unit sleeps. The unit sleeps between wakes and nestlocal
//! with it: a sensor event wakes it, and the log is read every POLL while
//! awake. nlclient rotates the log by replacing the file, which a changed
//! inode or a shorter file shows.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::bridge::Bridge;
use crate::logf;

const LOG: &str = "/media/log/nlclient";

/// How often the log is read while the unit is awake.
const POLL: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensor {
    /// The PIR: motion anywhere in view.
    Motion,
    /// The near-field proximity sensor: someone in front of the unit.
    Near,
}

impl Sensor {
    pub const ALL: [Sensor; 2] = [Sensor::Motion, Sensor::Near];

    pub fn key(self) -> &'static str {
        match self {
            Sensor::Motion => "motion",
            Sensor::Near => "near",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sensor::Motion => "Motion",
            Sensor::Near => "Near presence",
        }
    }

    pub fn device_class(self) -> &'static str {
        match self {
            Sensor::Motion => "motion",
            Sensor::Near => "occupancy",
        }
    }

    /// Seconds a sensor stays on after its last event. Proximity events
    /// come about every 15 seconds while someone stands in front of the
    /// unit, so a shorter hold flickers off between them.
    pub fn hold(self) -> u64 {
        30
    }
}

/// The sensor event a line of nlclient's log reports, if any.
pub fn event(line: &str) -> Option<Sensor> {
    if !line.contains("backplate controller:") {
        None
    } else if line.contains("publishing pir msg true") {
        Some(Sensor::Motion)
    } else if line.contains("tp crossings") {
        Some(Sensor::Near)
    } else {
        None
    }
}

/// Reads what was appended to a log since the last read, whole lines only,
/// starting from wherever the log ended when it was first opened.
struct Follower {
    path: PathBuf,
    inode: u64,
    pos: u64,
    partial: String,
}

impl Follower {
    fn open(path: PathBuf) -> Option<Follower> {
        let m = std::fs::metadata(&path).ok()?;
        Some(Follower { path, inode: m.ino(), pos: m.len(), partial: String::new() })
    }

    fn lines(&mut self) -> Vec<String> {
        let Ok(m) = std::fs::metadata(&self.path) else { return Vec::new() };
        if m.ino() != self.inode || m.len() < self.pos {
            // Rotated: the new file is read from its start.
            self.inode = m.ino();
            self.pos = 0;
            self.partial.clear();
        }
        if m.len() == self.pos {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        let read = std::fs::File::open(&self.path).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.pos))?;
            f.take(m.len() - self.pos).read_to_end(&mut bytes)
        });
        let Ok(n) = read else { return Vec::new() };
        self.pos += n as u64;
        self.partial.push_str(&String::from_utf8_lossy(&bytes));
        let complete = self.partial.rfind('\n').map_or(0, |i| i + 1);
        let lines = self.partial[..complete].lines().map(str::to_string).collect();
        self.partial.drain(..complete);
        lines
    }
}

/// Reports the unit's sensor events to the bridge for as long as nestlocal
/// runs, if nlclient's log is there to follow.
pub fn start(bridge: Arc<Bridge>) {
    let Some(mut log) = Follower::open(LOG.into()) else {
        logf!("motion: no {LOG} to follow");
        return;
    };
    logf!("motion: following {LOG}");
    bridge.has_sensors();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            let mut seen = Vec::new();
            for sensor in log.lines().iter().filter_map(|l| event(l)) {
                if !seen.contains(&sensor) {
                    seen.push(sensor);
                }
            }
            for sensor in seen {
                bridge.sensor_event(sensor);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIR: &str = "2026-11-06 23:10:38.134457 09AB01AC36230301 nlclient[500:bckpla] (2) backplate controller: publishing pir msg true (0x0001 0x7c00), threshold 100";
    const TP: &str = "2026-11-06 23:12:07.146671 09AB01AC36230301 nlclient[500:bckpla] (2) backplate controller: tp crossings - 22";
    const READING: &str = "2026-11-06 23:12:08.476949 09AB01AC36230301 nlclient[500:bckpla] (2) backplate controller: pir 7954; px1 65389/0, px2 65389/0; alv 0, alir 0";

    #[test]
    fn the_pir_is_motion_and_proximity_crossings_are_near_presence() {
        assert_eq!(event(PIR), Some(Sensor::Motion));
        assert_eq!(event(TP), Some(Sensor::Near));
        assert_eq!(event(READING), None, "a raw reading is not an event");
        assert_eq!(event("czcomms: promoting hot water schedule"), None);
    }

    #[test]
    fn the_follower_reads_new_whole_lines_and_restarts_on_rotation() {
        let dir = std::env::temp_dir().join(format!("nestlocal-motion-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nlclient");
        std::fs::write(&path, "old line\n").unwrap();
        let mut f = Follower::open(path.clone()).unwrap();
        assert!(f.lines().is_empty(), "what was there before is skipped");

        let append = |s: &str| {
            use std::io::Write;
            std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(s.as_bytes()).unwrap();
        };
        append(&format!("{PIR}\nhalf a li"));
        assert_eq!(f.lines(), vec![PIR.to_string()]);
        append("ne\n");
        assert_eq!(f.lines(), vec!["half a line".to_string()]);

        let rotated = dir.join("nlclient.new");
        std::fs::write(&rotated, format!("{TP}\n")).unwrap();
        std::fs::rename(&rotated, &path).unwrap();
        assert_eq!(f.lines(), vec![TP.to_string()]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
