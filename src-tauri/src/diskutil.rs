//! `diskutil` plist parsing. Used by both the app and the privileged helper.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Disk {
    /// e.g. "disk4"
    pub id: String,
    /// e.g. "/dev/disk4"
    pub device: String,
    /// MediaName
    pub name: String,
    /// TotalSize
    pub size_bytes: u64,
    /// BusProtocol, e.g. "USB", "Secure Digital"
    pub bus: String,
    pub removable: bool,
    pub volumes: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct ListPlist {
    whole_disks: Vec<String>,
    all_disks_and_partitions: Vec<DiskEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct DiskEntry {
    device_identifier: String,
    content: String,
    partitions: Vec<PartEntry>,
    #[serde(rename = "APFSVolumes")]
    apfs_volumes: Vec<PartEntry>,
    #[serde(rename = "APFSPhysicalStores")]
    apfs_physical_stores: Vec<PartEntry>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PartEntry {
    device_identifier: String,
    volume_name: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct InfoPlist {
    device_identifier: String,
    device_node: String,
    media_name: String,
    total_size: u64,
    bus_protocol: String,
    internal: Option<bool>,
    removable: bool,
    removable_media: bool,
    ejectable: bool,
    parent_whole_disk: String,
    #[serde(rename = "APFSPhysicalStores")]
    apfs_physical_stores: Vec<PhysicalStore>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
struct PhysicalStore {
    #[serde(rename = "APFSPhysicalStore")]
    apfs_physical_store: String,
}

/// True for whole-disk ids like "disk4" (`^disk\d+$`).
pub fn is_whole_disk_id(id: &str) -> bool {
    id.strip_prefix("disk")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// "disk0s2" / "disk3s1s1" -> "disk0" / "disk3".
fn whole_disk_of(id: &str) -> Option<String> {
    let rest = id.strip_prefix("disk")?;
    let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!n.is_empty()).then(|| format!("disk{n}"))
}

fn diskutil(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("/usr/sbin/diskutil")
        .args(args)
        .output()
        .with_context(|| format!("running diskutil {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "diskutil {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// Include virtual disks (RAM disks, disk images) for testing. Debug builds only.
fn allow_virtual() -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var("FLASHOS_ALLOW_VIRTUAL").as_deref() == Ok("1")
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

fn list_plist() -> Result<ListPlist> {
    let args: &[&str] = if allow_virtual() {
        &["list", "-plist", "external"]
    } else {
        &["list", "-plist", "external", "physical"]
    };
    parse_list(&diskutil(args)?)
}

fn parse_list(bytes: &[u8]) -> Result<ListPlist> {
    plist::from_bytes(bytes).context("parsing diskutil list plist")
}

fn parse_info(bytes: &[u8]) -> Result<InfoPlist> {
    plist::from_bytes(bytes).context("parsing diskutil info plist")
}

fn info(id: &str) -> Result<InfoPlist> {
    parse_info(&diskutil(&["info", "-plist", id])?)
}

/// Whole disks that are real flash targets: skips synthesized APFS containers.
fn candidate_ids(list: &ListPlist) -> Vec<String> {
    list.whole_disks
        .iter()
        .filter(|id| is_whole_disk_id(id))
        .filter(|id| {
            !list.all_disks_and_partitions.iter().any(|e| {
                &e.device_identifier == *id
                    && (!e.apfs_physical_stores.is_empty() || e.content == "Apple_APFS_Container")
            })
        })
        .cloned()
        .collect()
}

/// Volume names on `id`, including APFS volumes in containers backed by its partitions.
fn volumes_of(list: &ListPlist, id: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut push = |p: &PartEntry| {
        if let Some(n) = p.volume_name.as_deref().filter(|n| !n.is_empty()) {
            names.push(n.to_string());
        }
    };
    for e in &list.all_disks_and_partitions {
        let own = e.device_identifier == id;
        let backed = e
            .apfs_physical_stores
            .iter()
            .any(|s| whole_disk_of(&s.device_identifier).as_deref() == Some(id));
        if own {
            e.partitions.iter().for_each(&mut push);
        }
        if own || backed {
            e.apfs_volumes.iter().for_each(&mut push);
        }
    }
    names
}

fn to_disk(list: &ListPlist, i: InfoPlist) -> Disk {
    let volumes = volumes_of(list, &i.device_identifier);
    Disk {
        device: if i.device_node.is_empty() {
            format!("/dev/{}", i.device_identifier)
        } else {
            i.device_node
        },
        name: i.media_name,
        size_bytes: i.total_size,
        bus: i.bus_protocol,
        removable: i.removable || i.removable_media || i.ejectable,
        volumes,
        id: i.device_identifier,
    }
}

pub fn list_external_disks() -> Result<Vec<Disk>> {
    let list = list_plist()?;
    let mut disks = Vec::new();
    for id in candidate_ids(&list) {
        // A disk can vanish between list and info; skip it.
        let Ok(i) = info(&id) else { continue };
        if i.internal != Some(false) {
            continue;
        }
        disks.push(to_disk(&list, i));
    }
    Ok(disks)
}

/// Whole disks holding `/` (the container and its physical stores).
fn boot_disks(root: &InfoPlist) -> Vec<String> {
    let mut v: Vec<String> = root
        .apfs_physical_stores
        .iter()
        .filter_map(|s| whole_disk_of(&s.apfs_physical_store))
        .collect();
    v.extend(whole_disk_of(&root.parent_whole_disk));
    v.extend(whole_disk_of(&root.device_identifier));
    v
}

pub fn is_allowed_target(id: &str) -> Result<bool> {
    if !is_whole_disk_id(id) {
        return Ok(false);
    }
    let list = list_plist()?;
    if !candidate_ids(&list).iter().any(|d| d == id) {
        return Ok(false);
    }
    if info(id)?.internal != Some(false) {
        return Ok(false);
    }
    let root = info("/")?;
    Ok(!boot_disks(&root).iter().any(|d| d == id))
}

/// Look up one allowed disk (for size checks).
pub fn disk_info(id: &str) -> Result<Disk> {
    let list = list_plist()?;
    Ok(to_disk(&list, info(id)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST_DMG: &str = include_str!("../tests/fixtures/list_external_dmg.plist");
    const INFO_DMG: &str = include_str!("../tests/fixtures/info_disk4_dmg.plist");
    const INFO_ROOT: &str = include_str!("../tests/fixtures/info_root.plist");
    const LIST_USB: &str = include_str!("../tests/fixtures/list_external_usb.plist");
    const INFO_USB: &str = include_str!("../tests/fixtures/info_disk6_usb.plist");

    #[test]
    fn whole_disk_ids() {
        assert!(is_whole_disk_id("disk4"));
        assert!(is_whole_disk_id("disk12"));
        for bad in [
            "disk",
            "disk4s1",
            "rdisk4",
            "/dev/disk4",
            "disk4 ",
            "disk-1",
            "",
        ] {
            assert!(!is_whole_disk_id(bad), "{bad}");
        }
        assert_eq!(whole_disk_of("disk3s1s1").as_deref(), Some("disk3"));
        assert_eq!(whole_disk_of("foo"), None);
    }

    #[test]
    fn list_skips_synthesized_containers() {
        let list = parse_list(LIST_DMG.as_bytes()).unwrap();
        assert_eq!(list.whole_disks, vec!["disk4", "disk5"]);
        assert_eq!(candidate_ids(&list), vec!["disk4"]);
        assert_eq!(volumes_of(&list, "disk4"), vec!["Pi Desktop"]);
    }

    #[test]
    fn info_to_disk() {
        let list = parse_list(LIST_DMG.as_bytes()).unwrap();
        let i = parse_info(INFO_DMG.as_bytes()).unwrap();
        assert_eq!(i.internal, Some(false));
        let d = to_disk(&list, i);
        assert_eq!(d.id, "disk4");
        assert_eq!(d.device, "/dev/disk4");
        assert_eq!(d.name, "Disk Image");
        assert_eq!(d.size_bytes, 67087360);
        assert_eq!(d.bus, "Disk Image");
        assert!(d.removable);
    }

    #[test]
    fn usb_disk() {
        let list = parse_list(LIST_USB.as_bytes()).unwrap();
        assert_eq!(candidate_ids(&list), vec!["disk6"]);
        let d = to_disk(&list, parse_info(INFO_USB.as_bytes()).unwrap());
        assert_eq!(d.name, "SanDisk Ultra");
        assert_eq!(d.bus, "USB");
        assert_eq!(d.size_bytes, 30752636928);
        assert_eq!(d.volumes, vec!["bootfs"]);
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["sizeBytes"], 30752636928u64);
    }

    #[test]
    fn root_boot_disks() {
        let root = parse_info(INFO_ROOT.as_bytes()).unwrap();
        let boot = boot_disks(&root);
        assert!(boot.contains(&"disk0".to_string()));
        assert!(boot.contains(&"disk3".to_string()));
        assert_eq!(root.internal, Some(true));
    }
}
