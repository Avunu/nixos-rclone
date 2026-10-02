//! The two jobs that used to be bash in the module: staging credential-backed
//! rclone configs for FUSE mounts, and clearing stale FUSE mounts after resume.
//! (Bisync pairs receive their config as a unit credential instead; see
//! [`crate::pair`].)

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::fs::{AtFlags, CWD, StatxFlags, statx};
use rustix::mount::{UnmountFlags, unmount};

use crate::config::Config;

// ── Config staging ───────────────────────────────────────────────────────

/// Copy every credential-backed mount config into a writable location.
///
/// `.mount` units cannot use `LoadCredential`, and rclone persists token
/// refreshes into its config, which a read-only secret would reject on every
/// refresh ("Failed to save config after 10 tries").
pub fn stage_configs(cfg: &Config) -> Result<()> {
    for (name, m) in &cfg.mounts {
        if let Some(src) = &m.config_file {
            install_file(src, &cfg.staged_mount_config(name))
                .with_context(|| format!("staging config for mount {name}"))?;
        }
    }
    Ok(())
}

/// `install -m 600 src dst`, atomically: the file is never visible at `dst`
/// with the wrong mode, nor half written.
pub fn install_file(src: &Path, dst: &Path) -> Result<()> {
    let data = fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    let tmp = tmp_sibling(dst);
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(&data)?;
        f.sync_all()?;
        fs::rename(&tmp, dst).with_context(|| format!("renaming into {}", dst.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn tmp_sibling(dst: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(".");
    name.push(dst.file_name().unwrap_or_default());
    name.push(".tmp");
    dst.with_file_name(name)
}

// ── Mount reset ──────────────────────────────────────────────────────────

/// After resume, lazily unmount rclone FUSE mounts whose daemon died uncleanly
/// (the entry lingers, answers `ENOTCONN`, and blocks remounting), then clear
/// the failed state of their units so the automount can re-arm.
pub fn reset_mounts(cfg: &Config) -> Result<()> {
    std::thread::sleep(Duration::from_secs(cfg.mount_reset.delay));
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").context("reading mountinfo")?;

    for (name, m) in &cfg.mounts {
        // Reading mountinfo, unlike stat()ing the path, never triggers an armed
        // automount.
        if top_fstype(&mountinfo, &m.local_path).is_some_and(is_rclone_fstype)
            && !is_responsive(&m.local_path)
        {
            tracing::warn!(mount = %name, path = %m.local_path.display(), "stale rclone mount, detaching");
            if let Err(e) = unmount(&m.local_path, UnmountFlags::DETACH) {
                tracing::error!(mount = %name, error = %e, "lazy unmount failed");
            }
        }
        reset_failed(&m.unit);
    }
    Ok(())
}

fn is_rclone_fstype(t: &str) -> bool {
    t == "fuse.rclone" || t == "rclone"
}

/// Whether `path` can still be stat()ed. `AT_NO_AUTOMOUNT` so that probing can
/// never itself trigger an automount.
fn is_responsive(path: &Path) -> bool {
    statx(CWD, path, AtFlags::NO_AUTOMOUNT, StatxFlags::BASIC_STATS).is_ok()
}

fn reset_failed(unit: &str) {
    // Failure here is expected whenever the units are not in a failed state.
    let status = Command::new("systemctl")
        .args([
            "reset-failed",
            &format!("{unit}.mount"),
            &format!("{unit}.automount"),
        ])
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(e) = status {
        tracing::warn!(unit, error = %e, "could not run systemctl reset-failed");
    }
}

/// Filesystem type of the *topmost* mount at `path`, i.e. the last matching
/// line of `/proc/self/mountinfo`. An automount leaves an `autofs` entry
/// beneath the real one, so the last entry is the one that matters.
pub fn top_fstype<'a>(mountinfo: &'a str, path: &Path) -> Option<&'a str> {
    let want = path.as_os_str().as_encoded_bytes();
    let mut found = None;
    for line in mountinfo.lines() {
        let mut fields = line.split(' ');
        // id parent major:minor root mountpoint options [optional...] - fstype source super
        let Some(mountpoint) = fields.nth(4) else {
            continue;
        };
        if unescape_octal(mountpoint) != want {
            continue;
        }
        let mut rest = fields.skip_while(|f| *f != "-");
        if rest.next().is_some()
            && let Some(fstype) = rest.next()
        {
            found = Some(fstype);
        }
    }
    found
}

/// mountinfo escapes space, tab, newline and backslash as `\040` `\011`
/// `\012` `\134`.
fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') as u32 * 64
                + (b[i + 2] - b'0') as u32 * 8
                + (b[i + 3] - b'0') as u32;
            out.push(v as u8);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
24 1 0:21 / /mnt/test rw,relatime shared:5 - autofs systemd-1 rw,fd=30
97 24 0:50 / /mnt/test rw,nosuid,nodev,relatime shared:60 - fuse.rclone testremote:mountdir rw,user_id=0
30 1 0:25 / /mnt/my\\040docs rw,relatime - fuse.rclone webdav:docs rw
31 1 0:26 / /mnt/plain rw,relatime master:1 - ext4 /dev/sda1 rw
";

    #[test]
    fn topmost_mount_wins_over_autofs_underlay() {
        assert_eq!(
            top_fstype(MOUNTINFO, Path::new("/mnt/test")),
            Some("fuse.rclone")
        );
    }

    #[test]
    fn autofs_only_is_not_rclone() {
        let only_autofs = MOUNTINFO.lines().next().unwrap();
        assert_eq!(
            top_fstype(only_autofs, Path::new("/mnt/test")),
            Some("autofs")
        );
        assert!(!is_rclone_fstype("autofs"));
    }

    #[test]
    fn octal_escapes_in_mountpoints() {
        assert_eq!(
            top_fstype(MOUNTINFO, Path::new("/mnt/my docs")),
            Some("fuse.rclone")
        );
    }

    #[test]
    fn unrelated_and_missing_paths() {
        assert_eq!(top_fstype(MOUNTINFO, Path::new("/mnt/plain")), Some("ext4"));
        assert_eq!(top_fstype(MOUNTINFO, Path::new("/mnt/absent")), None);
        // A prefix of a mountpoint is not that mountpoint.
        assert_eq!(top_fstype(MOUNTINFO, Path::new("/mnt")), None);
    }

    #[test]
    fn unescape_leaves_plain_text_and_stray_backslashes() {
        assert_eq!(unescape_octal("/a/b"), b"/a/b");
        assert_eq!(unescape_octal("/a\\134b"), b"/a\\b");
        assert_eq!(unescape_octal("/a\\zz"), b"/a\\zz");
        assert_eq!(unescape_octal("/a\\04"), b"/a\\04");
    }

    #[test]
    fn staging_installs_private_files() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.conf");
        fs::write(&src, "[r]\ntype = local\n").unwrap();

        let json = format!(
            r#"{{"kind":"global","version":1,"stagingDir":"{d}/run",
                "mounts":{{"m":{{"localPath":"/mnt/m","unit":"mnt-m","configFile":"{s}"}},
                           "nocfg":{{"localPath":"/mnt/n","unit":"mnt-n","configFile":null}}}},
                "mountReset":{{"delay":0}}}}"#,
            d = dir.path().display(),
            s = src.display(),
        );
        fs::create_dir(dir.path().join("run")).unwrap();
        let cfg = crate::config::File::parse(&json)
            .unwrap()
            .into_global()
            .unwrap();
        stage_configs(&cfg).unwrap();
        // Idempotent: a restart re-stages over the existing files.
        stage_configs(&cfg).unwrap();

        let m = cfg.staged_mount_config("m");
        assert_eq!(fs::read_to_string(&m).unwrap(), "[r]\ntype = local\n");
        assert_eq!(fs::metadata(&m).unwrap().mode() & 0o777, 0o600);
        assert!(!cfg.staged_mount_config("nocfg").exists());
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(dir.path().join("run"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("m.conf")]);
    }
}
