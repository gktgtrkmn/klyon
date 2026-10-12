use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub size: u64,
    pub modified: i64,
    pub hash: Option<[u8; 32]>,
}

pub fn system_time_to_nanos(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_nanos())
            .map(|n| -n)
            .unwrap_or(i64::MIN),
    }
}

pub fn nanos_to_system_time(nanos: i64) -> SystemTime {
    if nanos >= 0 {
        UNIX_EPOCH + Duration::from_nanos(nanos as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(nanos.unsigned_abs())
    }
}

pub type Snapshot = BTreeMap<String, FileMeta>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    Added,
    Modified,
    Deleted,
}

pub fn compare_snapshots(old: &Snapshot, new: &Snapshot) -> BTreeMap<String, Alert> {
    let mut diffs: BTreeMap<String, Alert> = BTreeMap::new();

    for path in new.keys().filter(|k| !old.contains_key(*k)) {
        diffs.insert(path.clone(), Alert::Added);
    }

    for path in old.keys().filter(|k| !new.contains_key(*k)) {
        diffs.insert(path.clone(), Alert::Deleted);
    }

    for (path, old_meta) in old.iter() {
        if let Some(new_meta) = new.get(path) {
            if old_meta != new_meta {
                diffs.insert(path.clone(), Alert::Modified);
            }
        }
    }

    diffs
}
