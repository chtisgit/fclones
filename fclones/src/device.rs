use core::cmp;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::ops::Index;

use itertools::Itertools;
use lazy_init::Lazy;
use rayon::{ThreadPool, ThreadPoolBuilder};
use sysinfo::DiskKind;
#[cfg(not(target_os = "linux"))]
use sysinfo::{DiskExt, System, SystemExt};

use crate::config::Parallelism;
use crate::file::FileLen;
use crate::path::Path;

impl Parallelism {
    pub fn default_for(disk_kind: DiskKind) -> Parallelism {
        let cpu_count = num_cpus::get();
        match disk_kind {
            // SSDs typically benefit from a lot of parallelism.
            // Some users will probably want to increase it even more.
            DiskKind::SSD => Parallelism {
                random: 4 * cpu_count,
                sequential: 4 * cpu_count,
            },
            // Rotational drives can't serve multiple requests at once.
            // After introducing access ordering in fclones 0.9.0 it turns out
            // that we get slightly more IOPS when we schedule random access operations on a
            // single thread. For sequential scanning of big files, parallel access can hurt a lot,
            // so 1 is the only possible choice here.
            DiskKind::HDD => Parallelism {
                random: 1,
                sequential: 1,
            },
            // Unknown device here, so we need to stay away from potentially extremely bad defaults.
            // If the underlying device is an SSD, a single-threaded mode can
            // degrade random I/O performance many times. On the other hand making parallel random
            // access to a HDD didn't degrade performance by more than 30% in our tests,
            // and sometimes it can speed things up.
            // For sequential reads of big files we obviously stay single threaded,
            // as multithreading can hurt really a lot in case the underlying device is rotational.
            _ => Parallelism {
                random: 4 * cpu_count,
                sequential: 1,
            },
        }
    }
}

pub struct DiskDevice {
    pub index: usize,
    pub name: OsString,
    pub disk_kind: DiskKind,
    pub file_system: String,
    pub parallelism: Parallelism,
    seq_thread_pool: Lazy<ThreadPool>,
    rand_thread_pool: Lazy<ThreadPool>,
}

impl DiskDevice {
    fn new(
        index: usize,
        name: OsString,
        disk_kind: DiskKind,
        file_system: String,
        parallelism: Parallelism,
    ) -> DiskDevice {
        DiskDevice {
            index,
            name,
            disk_kind,
            file_system,
            parallelism,
            seq_thread_pool: Lazy::new(),
            rand_thread_pool: Lazy::new(),
        }
    }

    fn build_thread_pool(num_threads: usize) -> ThreadPool {
        ThreadPoolBuilder::default()
            .num_threads(num_threads)
            .build()
            .unwrap()
    }

    pub fn seq_thread_pool(&self) -> &ThreadPool {
        self.seq_thread_pool
            .get_or_create(|| Self::build_thread_pool(self.parallelism.sequential))
    }

    pub fn rand_thread_pool(&self) -> &ThreadPool {
        self.rand_thread_pool
            .get_or_create(|| Self::build_thread_pool(self.parallelism.random))
    }

    pub fn min_prefix_len(&self) -> FileLen {
        FileLen(match self.disk_kind {
            DiskKind::SSD => 4 * 1024,
            DiskKind::HDD => 4 * 1024,
            DiskKind::Unknown(_) => 4 * 1024,
        })
    }

    pub fn max_prefix_len(&self) -> FileLen {
        FileLen(match self.disk_kind {
            DiskKind::SSD => 4 * 1024,
            DiskKind::HDD => 16 * 1024,
            DiskKind::Unknown(_) => 16 * 1024,
        })
    }

    pub fn suffix_len(&self) -> FileLen {
        self.max_prefix_len()
    }

    pub fn suffix_threshold(&self) -> FileLen {
        FileLen(match self.disk_kind {
            DiskKind::HDD => 64 * 1024 * 1024, // 64 MB
            DiskKind::SSD => 64 * 1024,        // 64 kB
            DiskKind::Unknown(_) => 64 * 1024 * 1024,
        })
    }
}

/// A mounted file system as found on the system.
struct DiskInfo {
    name: OsString,
    kind: DiskKind,
    file_system: String,
    mount_point: std::path::PathBuf,
    removable: bool,
}

#[cfg(not(target_os = "linux"))]
fn list_disks() -> Vec<DiskInfo> {
    let mut sys = System::new();
    sys.refresh_disks_list();
    sys.disks()
        .iter()
        .map(|d| DiskInfo {
            name: d.name().to_os_string(),
            kind: d.kind(),
            file_system: String::from_utf8_lossy(d.file_system()).to_string(),
            mount_point: d.mount_point().to_path_buf(),
            removable: d.is_removable(),
        })
        .collect()
}

/// On Linux the mounts are read from /proc/mounts directly instead of through sysinfo:
/// sysinfo (0.29) calls statvfs() on every mount point, including autofs/systemd-automount
/// trigger points, which mounts them, or hangs when the target is unreachable - before fclones
/// even starts scanning. fclones only needs the mount table and the disk type, so no file system
/// is touched here.
#[cfg(target_os = "linux")]
fn list_disks() -> Vec<DiskInfo> {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    let removable = linux::removable_devices();
    linux::parse_mounts(&mounts)
        .into_iter()
        .map(|(spec, mount_point, fs_type)| DiskInfo {
            kind: linux::disk_kind(OsStr::new(&spec)),
            removable: removable.iter().any(|r| r.as_os_str() == OsStr::new(&spec)),
            name: OsString::from(spec),
            file_system: fs_type,
            mount_point: std::path::PathBuf::from(mount_point),
        })
        .collect()
}

#[cfg(target_os = "linux")]
mod linux {
    //! Mount table parsing and disk type detection, following sysinfo 0.29 (MIT license,
    //! https://github.com/GuillaumeGomez/sysinfo), but without calling statvfs().

    use std::ffi::OsStr;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use sysinfo::DiskKind;

    /// Parses /proc/mounts into (device, mount point, file system type), skipping pseudo
    /// file systems, network file systems and autofs trigger points.
    pub fn parse_mounts(content: &str) -> Vec<(String, String, String)> {
        let unescape = |s: &str| {
            s.replace("\\134", "\\")
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
        };
        content
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                let spec = unescape(fields.next()?);
                let file = unescape(fields.next()?);
                let fs_type = fields.next()?.to_string();
                Some((spec, file, fs_type))
            })
            .filter(|(spec, file, fs_type)| {
                let ignored_fs = matches!(
                    fs_type.as_str(),
                    "rootfs"
                        | "sysfs"
                        | "proc"
                        | "tmpfs"
                        | "devtmpfs"
                        | "cgroup"
                        | "cgroup2"
                        | "pstore"
                        | "squashfs"
                        | "rpc_pipefs"
                        | "iso9660"
                        | "nfs4"
                        | "nfs"
                        // automount trigger points: touching them mounts the target
                        | "autofs"
                );
                !(ignored_fs
                    || file.starts_with("/sys")
                    || file.starts_with("/proc")
                    || (file.starts_with("/run") && !file.starts_with("/run/media"))
                    || spec.starts_with("sunrpc"))
            })
            .collect()
    }

    /// Devices with a "usb-" link in /dev/disk/by-id.
    pub fn removable_devices() -> Vec<PathBuf> {
        match fs::read_dir("/dev/disk/by-id/") {
            Ok(r) => r
                .filter_map(|e| Some(e.ok()?.path()))
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("usb-"))
                })
                .filter_map(|p| p.canonicalize().ok())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Determines whether the device is rotational, from /sys/block/<disk>/queue/rotational.
    pub fn disk_kind(device_name: &OsStr) -> DiskKind {
        let device_name_path = device_name.to_str().unwrap_or_default();
        let real_path =
            fs::canonicalize(device_name).unwrap_or_else(|_| PathBuf::from(device_name));
        let mut real_path = real_path.to_str().unwrap_or_default();
        if device_name_path.starts_with("/dev/mapper/") || device_name_path.starts_with("/dev/root")
        {
            if real_path != device_name_path {
                return disk_kind(OsStr::new(&real_path));
            }
        } else if device_name_path.starts_with("/dev/sd") || device_name_path.starts_with("/dev/vd")
        {
            real_path = real_path.trim_start_matches("/dev/");
            real_path = real_path.trim_end_matches(|c: char| c.is_ascii_digit());
        } else if device_name_path.starts_with("/dev/nvme")
            || device_name_path.starts_with("/dev/mmcblk")
        {
            real_path = match real_path.find('p') {
                Some(idx) => &real_path["/dev/".len()..idx],
                None => &real_path["/dev/".len()..],
            };
        } else {
            real_path = real_path.trim_start_matches("/dev/");
        }
        let path = Path::new("/sys/block/")
            .join(OsStr::from_bytes(real_path.as_bytes()))
            .join("queue/rotational");
        match fs::read_to_string(path)
            .unwrap_or_default()
            .trim()
            .parse::<isize>()
        {
            Ok(1) => DiskKind::HDD,
            Ok(0) => DiskKind::SSD,
            Ok(x) => DiskKind::Unknown(x),
            Err(_) => DiskKind::Unknown(-1),
        }
    }
}

/// Finds disk devices by file paths
pub struct DiskDevices {
    devices: Vec<DiskDevice>,
    mount_points: Vec<(Path, usize)>,
}

impl DiskDevices {
    #[cfg(test)]
    pub fn single(disk_kind: DiskKind, parallelism: usize) -> DiskDevices {
        let device = DiskDevice::new(
            0,
            OsString::from("/"),
            disk_kind,
            String::from("unknown"),
            Parallelism {
                random: parallelism,
                sequential: parallelism,
            },
        );
        DiskDevices {
            devices: vec![device],
            mount_points: vec![(Path::from("/"), 0)],
        }
    }

    /// Reads the preferred parallelism level for the device based on the
    /// device name or the device type (ssd/hdd) from `pool_sizes` map.
    /// Returns the value under the "default" key if device was not found,
    /// or 0 if "default" doesn't exist in the map.
    /// If found, the device key is removed from the map.
    fn get_parallelism(
        name: &OsStr,
        disk_kind: DiskKind,
        removable: bool,
        pool_sizes: &HashMap<OsString, Parallelism>,
    ) -> Parallelism {
        let mut dev_key = OsString::new();
        dev_key.push("dev:");
        dev_key.push(name);
        match pool_sizes.get(&dev_key) {
            Some(p) => *p,
            None => {
                // "removable" (documented for --threads) takes precedence over the disk kind
                let removable_pool = removable.then(|| pool_sizes.get(OsStr::new("removable")));
                let p = removable_pool.flatten().or(match disk_kind {
                    DiskKind::SSD => pool_sizes.get(OsStr::new("ssd")),
                    DiskKind::HDD => pool_sizes.get(OsStr::new("hdd")),
                    DiskKind::Unknown(_) => pool_sizes.get(OsStr::new("unknown")),
                });
                match p {
                    Some(p) => *p,
                    None => pool_sizes
                        .get(OsStr::new("default"))
                        .copied()
                        .unwrap_or_else(|| Parallelism::default_for(disk_kind)),
                }
            }
        }
    }

    /// If the device doesn't exist, adds a new device to devices vector and returns its index.
    /// If the device already exists, it returns the index of the existing device.
    fn add_device(
        &mut self,
        name: OsString,
        disk_kind: DiskKind,
        removable: bool,
        file_system: String,
        pool_sizes: &HashMap<OsString, Parallelism>,
    ) -> usize {
        if let Some((index, _)) = self.devices.iter().find_position(|d| d.name == name) {
            index
        } else {
            let index = self.devices.len();
            let parallelism = Self::get_parallelism(&name, disk_kind, removable, pool_sizes);
            self.devices.push(DiskDevice::new(
                index,
                name,
                disk_kind,
                file_system,
                parallelism,
            ));
            index
        }
    }

    /// If `name` is a disk partition, it attempts to return the disk device name the partition
    /// resides on. Otherwise, and on failures, it just returns the same `name`.
    #[cfg(target_os = "linux")]
    fn physical_device_name(name: &OsStr) -> OsString {
        let regex =
            regex::Regex::new(r"^/dev/((?:[fhsv]|xv)d[a-z]+|nvme[0-9]+|mmcblk[0-9]+).*").unwrap();
        let name_str = name.to_string_lossy();
        match regex.captures(name_str.as_ref()) {
            Some(captures) => {
                let parent = "/dev/".to_owned() + captures.get(1).unwrap().as_str();
                OsString::from(parent)
            }
            None => name.to_os_string(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn physical_device_name(name: &OsStr) -> OsString {
        name.to_os_string()
    }

    /// Reads the list of partitions and disks from the system and builds the `DiskDevices`
    /// structure from that information.
    pub fn new(pool_sizes: &HashMap<OsString, Parallelism>) -> DiskDevices {
        let mut result = DiskDevices {
            devices: Vec::new(),
            mount_points: Vec::new(),
        };

        // Default device used when we don't find any real device
        result.add_device(
            OsString::from("default"),
            DiskKind::Unknown(-1),
            false,
            String::from("unknown"),
            pool_sizes,
        );
        for d in list_disks() {
            let device_name = Self::physical_device_name(&d.name);
            let index = result.add_device(
                device_name,
                d.kind,
                d.removable,
                d.file_system.clone(),
                pool_sizes,
            );

            // On macOS APFS disk users' data is mounted in '/System/Volumes/Data'
            // but fused transparently and presented as part of the root filesystem.
            // It requires remapping Data volume path for this DiskDevice to '/'.
            // https://www.swiftforensics.com/2019/10/macos-1015-volumes-firmlink-magic.html
            // https://eclecticlight.co/2020/01/23/catalina-boot-volumes/
            if cfg!(target_os = "macos")
                && d.file_system == "apfs"
                && d.mount_point.to_str() == Some("/System/Volumes/Data")
            {
                result.mount_points.push((Path::from("/"), index));
            } else {
                result
                    .mount_points
                    .push((Path::from(&d.mount_point), index));
            };
        }
        result
            .mount_points
            .sort_by_key(|(p, _)| cmp::Reverse(p.component_count()));

        result
    }

    /// Returns the mount point holding given path, or `None` if it is not known
    /// (e.g. when no disks could be detected).
    /// Mount points are sorted from the most nested one, so the first prefix is the right one.
    pub fn get_mount_point(&self, path: &Path) -> Option<&Path> {
        self.mount_points
            .iter()
            .map(|(p, _)| p)
            .find(|p| p.is_prefix_of(path))
    }

    /// Returns the disk device which holds the given path
    pub fn get_by_path(&self, path: &Path) -> &DiskDevice {
        self.mount_points
            .iter()
            .find(|(p, _)| p.is_prefix_of(path))
            .map(|&(_, index)| &self.devices[index])
            .unwrap_or(&self.devices[0])
    }

    /// Returns the disk device by its device name (not mount point)
    pub fn get_by_name(&self, name: &OsStr) -> Option<&DiskDevice> {
        self.devices.iter().find(|&d| d.name == name)
    }

    /// Returns the first device on the list
    pub fn get_default(&self) -> &DiskDevice {
        &self.devices[0]
    }

    /// Returns the number of devices
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Always returns false, because the default device is guaranteed to exist
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// Returns an iterator over devices
    pub fn iter(&self) -> impl Iterator<Item = &DiskDevice> {
        self.devices.iter()
    }

    /// Returns device_group identifiers recognized by the constructor
    pub fn device_types() -> Vec<&'static str> {
        vec!["ssd", "hdd", "removable", "unknown"]
    }
}

impl Default for DiskDevices {
    fn default() -> Self {
        let pool_sizes = HashMap::new();
        Self::new(&pool_sizes)
    }
}

impl Index<usize> for DiskDevices {
    type Output = DiskDevice;

    fn index(&self, index: usize) -> &Self::Output {
        &self.devices[index]
    }
}

#[cfg(test)]
mod test {
    use super::*;

    /// autofs / systemd-automount trigger points must not be listed (and so never touched):
    /// sysinfo called statvfs() on them, which mounts or hangs (upstream issues #323, #337).
    #[test]
    #[cfg(target_os = "linux")]
    fn mounts_skip_autofs_and_pseudo_file_systems() {
        let mounts = "/dev/sda1 / ext4 rw 0 0\n\
            systemd-1 /mnt/usb autofs rw,fd=67 0 0\n\
            /dev/sdb1 /mnt/usb ext4 rw 0 0\n\
            server:/export /srv/nfs nfs4 rw 0 0\n\
            proc /proc proc rw 0 0\n\
            /dev/sdc1 /media/My\\040Disk vfat rw 0 0\n";
        let parsed = linux::parse_mounts(mounts);
        let mount_points: Vec<&str> = parsed.iter().map(|(_, m, _)| m.as_str()).collect();
        assert_eq!(mount_points, vec!["/", "/mnt/usb", "/media/My Disk"]);
        assert_eq!(parsed[1].0, "/dev/sdb1"); // the real mount, not the autofs trigger
    }

    /// The own mount table must contain every disk sysinfo reports, with the same device name,
    /// disk type and file system (sysinfo additionally drops file systems of size 0).
    #[test]
    #[cfg(target_os = "linux")]
    fn own_disk_list_matches_sysinfo() {
        use sysinfo::{DiskExt, System, SystemExt};
        let mut sys = System::new();
        sys.refresh_disks_list();
        let ours = list_disks();
        for d in sys.disks() {
            let found = ours.iter().find(|o| o.mount_point == d.mount_point());
            let o = found.unwrap_or_else(|| panic!("missing {:?}", d.mount_point()));
            assert_eq!(o.name, d.name(), "{:?}", d.mount_point());
            assert_eq!(o.kind, d.kind(), "{:?}", d.mount_point());
            assert_eq!(
                o.file_system.as_bytes(),
                d.file_system(),
                "{:?}",
                d.mount_point()
            );
            assert_eq!(o.removable, d.is_removable(), "{:?}", d.mount_point());
        }
    }

    /// `--threads removable:N` was accepted but never used.
    #[test]
    fn removable_pool_size_is_used() {
        let mut pool_sizes = HashMap::new();
        let removable = Parallelism {
            random: 3,
            sequential: 2,
        };
        pool_sizes.insert(OsString::from("removable"), removable);
        let p =
            DiskDevices::get_parallelism(OsStr::new("/dev/sdz"), DiskKind::HDD, true, &pool_sizes);
        assert_eq!((p.random, p.sequential), (3, 2));
        let p =
            DiskDevices::get_parallelism(OsStr::new("/dev/sdz"), DiskKind::HDD, false, &pool_sizes);
        assert_eq!((p.random, p.sequential), (1, 1)); // HDD default
    }

    /// Without any detected mount point (e.g. in some containers) this used to panic with an
    /// out-of-bounds index; the fallback was also the most nested mount, not a sensible one.
    #[test]
    fn mount_point_lookup() {
        let mut devices = DiskDevices::single(DiskKind::SSD, 1);
        assert!(!devices.is_empty()); // used to panic: the assertion was inverted
        devices.mount_points.clear();
        assert_eq!(devices.get_mount_point(&Path::from("/a/b")), None);

        devices.mount_points = vec![(Path::from("/home"), 0), (Path::from("/"), 0)];
        assert_eq!(
            devices.get_mount_point(&Path::from("/home/x")),
            Some(&Path::from("/home"))
        );
        assert_eq!(
            devices.get_mount_point(&Path::from("/homework/x")),
            Some(&Path::from("/"))
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore)]
    fn test_physical_device_name() {
        assert_eq!(
            DiskDevices::physical_device_name(OsStr::new("/dev/sda")),
            OsString::from("/dev/sda")
        );
        assert_eq!(
            DiskDevices::physical_device_name(OsStr::new("/dev/sda1")),
            OsString::from("/dev/sda")
        );
        assert_eq!(
            DiskDevices::physical_device_name(OsStr::new("/dev/hdc20")),
            OsString::from("/dev/hdc")
        );
        assert_eq!(
            DiskDevices::physical_device_name(OsStr::new("/dev/nvme0n1p3")),
            OsString::from("/dev/nvme0")
        );
        assert_eq!(
            DiskDevices::physical_device_name(OsStr::new("/dev/unknown")),
            OsString::from("/dev/unknown")
        );
        // multi-letter, virtio, Xen and MMC devices (used to be mapped wrongly or not at all)
        for (partition, disk) in [
            ("/dev/sdaa1", "/dev/sdaa"),
            ("/dev/vda2", "/dev/vda"),
            ("/dev/xvdb1", "/dev/xvdb"),
            ("/dev/mmcblk0p1", "/dev/mmcblk0"),
        ] {
            assert_eq!(
                DiskDevices::physical_device_name(OsStr::new(partition)),
                OsString::from(disk)
            );
        }
    }
}
