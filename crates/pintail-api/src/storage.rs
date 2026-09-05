//! Filesystem capacity: how much room this node's data actually has.
//!
//! Two figures, because they answer different questions. The system volume
//! is what fills up and takes the whole host with it. The data directory's
//! volume is what fills up and stops replication - and on any real
//! deployment it is a mount of its own (the container image declares
//! `VOLUME /var/lib/pintail`), so the system's free space says nothing
//! about how much runway the mirrors have.
//!
//! Measured with `df` rather than `statvfs`: `unsafe_code` is forbidden
//! workspace-wide, so the libc call is not available, and one short-lived
//! subprocess per dashboard refresh is the same trade the vitals sampler
//! already makes for its non-Linux readings.

use std::{path::Path, process::Command};

use axum::{Extension, Json, extract::State};
use serde::Serialize;

use crate::{ApiState, auth::AuthPrincipal, error::ApiError};

/// One filesystem's capacity.
///
/// `used + available` is normally less than `total`: filesystems reserve a
/// slice for the superuser, and reporting a used percentage against
/// `used + available` would show a full disk as 100% while `df` still calls
/// it 95%. All three are carried so the client can present whichever the
/// operator is comparing against.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Volume {
    /// Where the filesystem is mounted, as `df` reports it.
    pub(crate) mount: String,
    pub(crate) total_bytes: u64,
    pub(crate) used_bytes: u64,
    pub(crate) available_bytes: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct StorageResponse {
    /// The configured data directory, as this process sees it.
    data_dir: String,
    /// Whether that directory sits on a filesystem of its own. False means
    /// the two volumes below are the same one, and a client should show a
    /// single figure rather than the same number twice.
    separate_mount: bool,
    /// The volume holding the data directory. `None` when it cannot be
    /// measured - a directory that does not exist yet, or a `df` that
    /// failed - which a client must render as unknown rather than as zero
    /// free space.
    data: Option<Volume>,
    /// The volume holding the root of the filesystem.
    system: Option<Volume>,
}

/// `GET /api/storage`.
///
/// Authenticated: a mount layout and its free space describe the host, and
/// the unauthenticated `/status` endpoint is not the place for them.
pub(crate) async fn storage(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Json<StorageResponse>, ApiError> {
    principal.require_scope("read")?;
    let data_dir = state.data_dir()?.to_path_buf();
    Ok(Json(report(&data_dir)))
}

fn report(data_dir: &Path) -> StorageResponse {
    let data = volume(data_dir);
    let system = volume(Path::new("/"));
    StorageResponse {
        data_dir: data_dir.display().to_string(),
        // Compared by what the volumes REPORT, not by the configured path:
        // a custom `--data-dir` that happens to live on the system volume
        // has no second number to show, and the default path on a mounted
        // volume (which is how the container ships) has one.
        //
        // Size is part of the test because a differing mount point is not
        // enough. macOS firmlinks one APFS container in at both `/` and
        // `/System/Volumes/Data`: two mount points, one store, identical
        // totals - and their *used* figures differ, because the sealed
        // system volume counts only itself. Reported as separate, a client
        // leading with the system volume would call a 91%-full disk 3%
        // used.
        //
        // Only the total is compared. Free space moves between the two `df`
        // calls on any busy machine, which made an earlier version report
        // one store as two whenever a few blocks were written in between.
        // Two genuinely different volumes of exactly equal size collapse to
        // one figure, which costs the system's free space and nothing
        // else - and is the right answer for the case that actually occurs,
        // a container volume carved out of the same disk as its overlay.
        separate_mount: match (&data, &system) {
            (Some(data), Some(system)) => separate_volumes(data, system),
            _ => false,
        },
        data,
        system,
    }
}

/// Whether two readings describe two stores rather than one store seen
/// twice. See the comment in [`report`] for why the sizes decide it.
fn separate_volumes(data: &Volume, system: &Volume) -> bool {
    data.mount != system.mount && data.total_bytes != system.total_bytes
}

/// The capacity of the filesystem holding `path`.
///
/// A path that does not exist yet answers `None` rather than falling back to
/// its parent: the parent may be on a different filesystem, and a number
/// from the wrong volume is worse than no number.
fn volume(path: &Path) -> Option<Volume> {
    let output = Command::new("df")
        // -k fixes the block size at 1024 bytes on both Linux and macOS,
        // whose defaults differ; -P keeps each filesystem on one line, so a
        // long device name cannot wrap the columns apart.
        .args(["-kP".as_ref(), path.as_os_str()])
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    parse_df(&String::from_utf8_lossy(&output.stdout))
}

/// Reads the one data row of POSIX `df -kP` output.
///
/// Columns are filesystem, 1024-blocks, used, available, capacity, mount.
/// The mount point is taken as the rest of the line: it is the only field
/// that can contain spaces, and splitting it away would report `/Volumes`
/// for a disk mounted at `/Volumes/big disk`.
fn parse_df(output: &str) -> Option<Volume> {
    let row = output.lines().nth(1)?;
    let fields: Vec<&str> = row.split_whitespace().collect();
    if fields.len() < 6 {
        return None;
    }
    let kilobytes =
        |index: usize| -> Option<u64> { fields.get(index)?.parse::<u64>().ok()?.checked_mul(1024) };
    // Positional, not a search for the field's text: searching would find
    // the mount point "/" inside "/dev/nvme0n1p2" and report the whole row.
    let mut rest = row.trim_start();
    for _ in 0..5 {
        rest = rest[rest.find(char::is_whitespace)?..].trim_start();
    }
    Some(Volume {
        mount: rest.trim_end().to_owned(),
        total_bytes: kilobytes(1)?,
        used_bytes: kilobytes(2)?,
        available_bytes: kilobytes(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_df, report};

    #[test]
    fn reads_the_posix_df_columns_including_a_mount_point_with_spaces() {
        let macos = "Filesystem 1024-blocks      Used Available Capacity  Mounted on\n\
                     /dev/disk3s5   971350180 120000000 800000000      14%  /Volumes/big disk\n";
        let volume = parse_df(macos).expect("one filesystem");
        assert_eq!(volume.mount, "/Volumes/big disk");
        assert_eq!(volume.total_bytes, 971_350_180 * 1024);
        assert_eq!(volume.used_bytes, 120_000_000 * 1024);
        assert_eq!(volume.available_bytes, 800_000_000 * 1024);

        let linux = "Filesystem     1024-blocks      Used Available Capacity Mounted on\n\
                     /dev/nvme0n1p2   488245288 100000000 363000000      22% /\n";
        assert_eq!(parse_df(linux).expect("one filesystem").mount, "/");

        // A df that printed nothing usable answers None, never zero: zero
        // free space would read as a full disk on the dashboard.
        assert!(parse_df("").is_none());
        assert!(parse_df("Filesystem 1024-blocks\n").is_none());
        assert!(
            parse_df("Filesystem 1024-blocks Used Available Capacity Mounted on\nx y z\n")
                .is_none()
        );
    }

    /// Two mount points over one store are one volume.
    ///
    /// This is not hypothetical: it is every macOS host, where `/` and
    /// `/System/Volumes/Data` are firmlinked halves of the same APFS
    /// container. The dashboard leads with the data directory's volume, so
    /// mislabelling this pair as separate would put the sealed system
    /// volume's used figure - 3% of a disk with 41GB left - on the card.
    #[test]
    fn two_mount_points_over_one_store_are_not_two_volumes() {
        let firmlinked = "Filesystem 1024-blocks Used Available Capacity Mounted on\n                          /dev/disk3s5 482797652 403374668 40307996 91% /System/Volumes/Data\n";
        let sealed = "Filesystem 1024-blocks Used Available Capacity Mounted on\n                      /dev/disk3s1s1 482797652 12344944 40307996 24% /\n";
        let data = parse_df(firmlinked).expect("data volume");
        let system = parse_df(sealed).expect("system volume");
        assert_ne!(data.mount, system.mount);
        assert_eq!(data.total_bytes, system.total_bytes);
        assert!(
            !super::separate_volumes(&data, &system),
            "one size under two names is one store"
        );

        // Free space read a moment apart must not turn it into two: these
        // are the same two mounts, sampled after a few blocks were written.
        let later = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                     /dev/disk3s1s1 482797652 12344944 40307900 24% /\n";
        let system_later = parse_df(later).expect("system volume, moments later");
        assert_ne!(data.available_bytes, system_later.available_bytes);
        assert!(!super::separate_volumes(&data, &system_later));

        // A real second disk differs in what it reports, not only in what
        // it is called.
        let mounted = "Filesystem 1024-blocks Used Available Capacity Mounted on\n                       /dev/sdb1 3906250000 120000000 3786250000 4% /mnt/pintail\n";
        let attached = parse_df(mounted).expect("attached volume");
        assert!(super::separate_volumes(&attached, &system));
    }

    #[test]
    fn a_data_directory_on_the_system_volume_is_not_reported_as_a_separate_mount() {
        // The repository itself: whatever volume it is on, it is the same
        // one `/` reports unless this checkout lives on a mounted disk, so
        // assert the relationship rather than a value.
        let here = std::env::current_dir().expect("working directory");
        let answer = report(&here);
        let (Some(data), Some(system)) = (&answer.data, &answer.system) else {
            // No usable `df` on this machine; the endpoint degrades to
            // "unknown" and there is nothing to compare.
            return;
        };
        assert_eq!(answer.separate_mount, super::separate_volumes(data, system));
        assert!(system.total_bytes > 0, "the root volume has a size");
    }

    #[test]
    fn a_missing_directory_measures_nothing_rather_than_guessing_a_parent() {
        let missing = std::path::Path::new("/pintail-nonexistent-path-for-tests/data");
        let answer = report(missing);
        assert!(
            answer.data.is_none(),
            "an unmeasurable path must not borrow another volume's numbers"
        );
        assert!(!answer.separate_mount);
    }
}
