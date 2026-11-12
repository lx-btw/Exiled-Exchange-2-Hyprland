//! Stable reader identities: an event node may be replaced or its fd reused.
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(crate) type DeviceId = u64;

/// uinput devices can advertise USB bus IDs, so check their actual sysfs ancestry.
pub(crate) fn is_physical(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let sysfs = Path::new("/sys/class/input").join(name).join("device");
    sysfs.canonicalize().is_ok_and(|device| {
        device.starts_with("/sys/devices") && !device.starts_with("/sys/devices/virtual")
    })
}

#[derive(Default)]
pub(crate) struct Devices {
    next: DeviceId,
    live: HashMap<DeviceId, (PathBuf, u64, u64)>,
}

impl Devices {
    pub fn attach(&mut self, path: &Path) -> Option<DeviceId> {
        let metadata = path.metadata().ok()?;
        self.insert((path.to_owned(), metadata.dev(), metadata.ino()))
    }

    fn insert(&mut self, identity: (PathBuf, u64, u64)) -> Option<DeviceId> {
        if self.live.values().any(|known| *known == identity) {
            return None;
        }
        self.next = self.next.checked_add(1).expect("input reader ID exhausted");
        self.live.insert(self.next, identity);
        Some(self.next)
    }

    pub fn contains(&self, id: DeviceId) -> bool {
        self.live.contains_key(&id)
    }

    pub fn remove(&mut self, id: DeviceId) {
        self.live.remove(&id);
    }

    pub fn ids(&self) -> Vec<DeviceId> {
        self.live.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deduplicates_and_never_reuses_reader_ids() {
        let mut devices = Devices::default();
        let identity = (PathBuf::from("event0"), 1, 2);
        let old = devices.insert(identity.clone()).unwrap();
        assert!(devices.insert(identity.clone()).is_none());
        devices.remove(old);
        let new = devices.insert(identity).unwrap();
        devices.remove(old); // A delayed Closed must not remove its replacement.
        assert!(devices.contains(new));
        assert_ne!(old, new);
    }

    #[test]
    fn replaced_node_can_attach_before_old_reader_closes() {
        let mut devices = Devices::default();
        let old = devices.insert((PathBuf::from("event0"), 1, 2)).unwrap();
        let new = devices.insert((PathBuf::from("event0"), 1, 3)).unwrap();
        devices.remove(old);
        assert_eq!(devices.ids(), vec![new]);
    }
}
