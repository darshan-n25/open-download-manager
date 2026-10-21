//! Everything that differs between operating systems, in one place.
//!
//! Open Download Manager runs on Linux, macOS and Windows, and the differences between them are small
//! enough to deserve one module rather than a `cfg` in every file:
//!
//! * **Where files live.** Windows has no `HOME`; it has `%USERPROFILE%`, and roaming
//!   config is not the same thing as local state. A `HOME`-only lookup would quietly put
//!   the config file and the download state in the current working directory, so the
//!   layout is spelled out per platform and resolved from the environment as a *pure
//!   function*, which lets both platforms' answers be tested from either one.
//! * **Positional writes.** `FileExt::write_at` (Unix) and `FileExt::seek_write` (Windows)
//!   are the same operation under two names. The retry loop around them is shared, so the
//!   logic that can actually be wrong is tested on every platform, and the platform-
//!   specific part is one line.
//! * **Reserved names.** Windows treats `NUL`, `CON`, `COM1` and friends as devices
//!   *anywhere* in a path, with or without an extension. A name that legitimately comes out
//!   of a URL is then not a file at all, so it has to be recognised before it is used.
//!
//! This module deliberately uses nothing but `std`. That makes it possible to *compile* the
//! Windows branches from Linux — `rustc --emit=metadata --target x86_64-pc-windows-msvc
//! src/platform.rs` needs only the target's prebuilt `std`, not a cross C toolchain — which
//! is what `scripts/check-portability.sh` does.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// An environment variable (or ordered list of them) that names a directory.
///
/// `from_env` is tried in order and the first variable that is set *and non-empty* wins;
/// otherwise the directory is `under_home`, relative to the user's home directory.
pub struct Base {
    pub from_env: &'static [&'static str],
    pub under_home: &'static str,
}

/// One platform's environment conventions.
pub struct Spec {
    /// Ways to find the home directory, in order of preference. A group of several names is
    /// concatenated (`HOMEDRIVE` + `HOMEPATH` is `C:\Users\me`), which is why this is a list
    /// of groups rather than a list of names.
    pub home: &'static [&'static [&'static str]],
    pub data: Base,
    pub config: Base,
    /// Our own directory name, inside the data and config bases.
    pub app: &'static str,
}

/// Everything a POSIX system needs, plus the XDG overrides that are honoured when set.
pub const UNIX: Spec = Spec {
    home: &[&["HOME"]],
    data: Base {
        from_env: &["XDG_DATA_HOME"],
        under_home: ".local/share",
    },
    config: Base {
        from_env: &["XDG_CONFIG_HOME"],
        under_home: ".config",
    },
    app: "odm",
};

/// The Windows conventions.
///
/// `LOCALAPPDATA` holds the download state and `APPDATA` holds the config, which is not a
/// detail: state points at the part files sitting on *this* disk, so letting it roam to a
/// machine that does not have them would hand the user a queue of downloads that cannot
/// resume.
pub const WINDOWS: Spec = Spec {
    home: &[&["USERPROFILE"], &["HOMEDRIVE", "HOMEPATH"]],
    data: Base {
        from_env: &["LOCALAPPDATA"],
        under_home: "AppData/Local",
    },
    config: Base {
        from_env: &["APPDATA"],
        under_home: "AppData/Roaming",
    },
    app: "odm",
};

/// The conventions of the platform this binary was built for.
pub fn spec() -> &'static Spec {
    if cfg!(windows) {
        &WINDOWS
    } else {
        &UNIX
    }
}

/// The directories one run uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    /// State: job/queue files, quota window, project documents, mirrors.
    pub data: PathBuf,
    /// The config file's directory.
    pub config: PathBuf,
}

impl Layout {
    /// Where a finished download goes by default: `Downloads` in the home directory, as
    /// every browser does and as IDM's catch-all category does.
    pub fn downloads(&self) -> PathBuf {
        self.home.join("Downloads")
    }
}

/// The directories for this process.
pub fn layout() -> Layout {
    layout_for(spec(), &|name| std::env::var_os(name))
}

/// Resolve a layout from an environment, without reading the real one.
///
/// Taking the environment as an argument is what makes the Windows answers testable on
/// Linux and vice versa.
fn layout_for(spec: &Spec, get: &dyn Fn(&str) -> Option<OsString>) -> Layout {
    // Last resort: no home directory anywhere. `.` is what the program used to do, and it
    // is wrong in a subtle way (the files move with the working directory), so it stays a
    // documented last resort rather than a silent one — `odm-cli config path` and
    // `odm-cli config show` print what was resolved.
    let home = spec
        .home
        .iter()
        .find_map(|group| concatenated(group, get))
        .unwrap_or_else(|| PathBuf::from("."));
    Layout {
        data: under(&spec.data, &home, spec.app, get),
        config: under(&spec.config, &home, spec.app, get),
        home,
    }
}

/// Concatenate a group of environment variables into one path, or `None` unless all of them
/// are set and the result is non-empty.
fn concatenated(group: &[&str], get: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let mut out = OsString::new();
    for name in group {
        out.push(non_empty(get, name)?);
    }
    let path = PathBuf::from(out);
    (!path.as_os_str().is_empty()).then_some(path)
}

/// `<base>/<app>`, where `<base>` is the first set variable or `<home>/<under_home>`.
fn under(base: &Base, home: &Path, app: &str, get: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    let root = base
        .from_env
        .iter()
        .find_map(|name| non_empty(get, name))
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(base.under_home));
    root.join(app)
}

/// The value of `name`, unless it is unset or empty.
///
/// An empty `XDG_DATA_HOME=` means "unset, use the default" in every implementation of the
/// standard, and joining onto an empty path would silently produce a relative one.
fn non_empty(get: &dyn Fn(&str) -> Option<OsString>, name: &str) -> Option<OsString> {
    get(name).filter(|v| !v.is_empty())
}

/// Write all of `buf` at `offset`, retrying short writes.
///
/// Positional writes never move the file cursor, so any number of workers can fill disjoint
/// regions of one file at the same time without locking.
pub fn write_all_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    write_all_at_with(buf, offset, |part, at| write_at(file, part, at))
}

#[cfg(unix)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

/// Windows spells the same operation `seek_write`. It is positional — despite the name it
/// does not disturb any shared file position — but it is a different trait method, so it
/// needs its own binding.
#[cfg(windows)]
fn write_at(file: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

/// The retry loop, written once and platform-independent.
///
/// Keeping it separate from the binding is not just tidiness: it is the part with logic in
/// it, so it is the part that gets tested — on every platform, including in CI where the
/// Windows build runs the same tests.
fn write_all_at_with<F>(buf: &[u8], mut offset: u64, mut put: F) -> std::io::Result<()>
where
    F: FnMut(&[u8], u64) -> std::io::Result<usize>,
{
    let mut written = 0usize;
    while written < buf.len() {
        let n = put(&buf[written..], offset)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short write",
            ));
        }
        if n > buf.len() - written {
            // Trusting an over-long count would advance past bytes that were never written
            // and leave a hole in the middle of the file.
            return Err(std::io::Error::other(
                "writer reported more bytes than it was given",
            ));
        }
        written += n;
        offset += n as u64;
    }
    Ok(())
}

/// True when `name` would address a device rather than a file on Windows.
///
/// Windows resolves `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9` and `LPT1`-`LPT9` in *any*
/// directory and with any extension — `NUL.txt` is still the null device — so a URL segment
/// or a `Content-Disposition` filename that happens to be one of them must be altered
/// before it is used as a path. The exact documented list is used here: `COM0` and `LPT0`
/// are ordinary names, and `COM10` is a file.
///
/// The trailing dots and spaces that Windows discards are removed first, because `NUL.` and
/// `COM1 ` are the same device as `NUL` and `COM1`.
pub fn is_reserved_windows_name(name: &str) -> bool {
    let name = name.trim_end_matches([' ', '.']);
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0'
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A layout resolver fed from a literal environment, so no test touches the real one.
    fn layout_of(spec: &Spec, vars: &[(&str, &str)]) -> Layout {
        let env: HashMap<&str, &str> = vars.iter().copied().collect();
        layout_for(spec, &|name: &str| env.get(name).map(OsString::from))
    }

    #[test]
    fn unix_uses_the_xdg_directories_when_they_are_set() {
        let l = layout_of(
            &UNIX,
            &[
                ("HOME", "/home/me"),
                ("XDG_DATA_HOME", "/srv/data"),
                ("XDG_CONFIG_HOME", "/srv/config"),
            ],
        );
        assert_eq!(l.home, PathBuf::from("/home/me"));
        assert_eq!(l.data, PathBuf::from("/srv/data/odm"));
        assert_eq!(l.config, PathBuf::from("/srv/config/odm"));
        assert_eq!(l.downloads(), PathBuf::from("/home/me/Downloads"));
    }

    #[test]
    fn unix_falls_back_to_the_home_directory() {
        let l = layout_of(&UNIX, &[("HOME", "/home/me")]);
        assert_eq!(l.data, PathBuf::from("/home/me/.local/share/odm"));
        assert_eq!(l.config, PathBuf::from("/home/me/.config/odm"));
    }

    #[test]
    fn windows_keeps_state_local_and_config_roaming() {
        let l = layout_of(
            &WINDOWS,
            &[
                ("USERPROFILE", r"C:\Users\me"),
                ("LOCALAPPDATA", r"C:\Users\me\AppData\Local"),
                ("APPDATA", r"C:\Users\me\AppData\Roaming"),
            ],
        );
        assert_eq!(l.home, PathBuf::from(r"C:\Users\me"));
        assert_eq!(
            l.data,
            PathBuf::from(r"C:\Users\me\AppData\Local").join("odm")
        );
        assert_eq!(
            l.config,
            PathBuf::from(r"C:\Users\me\AppData\Roaming").join("odm")
        );
        // The one path that must never be the working directory: Windows sets no `HOME`,
        // and a `HOME`-only lookup would have produced `.` here.
        assert_eq!(
            l.downloads(),
            PathBuf::from(r"C:\Users\me").join("Downloads")
        );
    }

    #[test]
    fn windows_falls_back_to_appdata_under_the_user_profile() {
        // A stripped-down environment — a service account, or `cmd` with a cleaned env.
        let l = layout_of(&WINDOWS, &[("USERPROFILE", r"C:\Users\me")]);
        assert_eq!(
            l.data,
            PathBuf::from(r"C:\Users\me")
                .join("AppData/Local")
                .join("odm")
        );
        assert_eq!(
            l.config,
            PathBuf::from(r"C:\Users\me")
                .join("AppData/Roaming")
                .join("odm")
        );
    }

    #[test]
    fn a_split_home_directory_is_rejoined() {
        // `HOMEDRIVE` ends in a colon and `HOMEPATH` starts with a separator, so plain
        // concatenation is the only correct join.
        let l = layout_of(&WINDOWS, &[("HOMEDRIVE", "C:"), ("HOMEPATH", r"\Users\me")]);
        assert_eq!(l.home, PathBuf::from(r"C:\Users\me"));
        assert_eq!(
            l.downloads(),
            PathBuf::from(r"C:\Users\me").join("Downloads")
        );
    }

    #[test]
    fn a_half_set_split_home_is_not_a_home() {
        // Just `HOMEDRIVE` is not a directory; falling through to the next candidate (and
        // then to the last resort) is better than using `C:`.
        let l = layout_of(&WINDOWS, &[("HOMEDRIVE", "C:")]);
        assert_eq!(l.home, PathBuf::from("."));
    }

    #[test]
    fn an_empty_variable_means_unset() {
        let l = layout_of(
            &UNIX,
            &[
                ("HOME", "/home/me"),
                ("XDG_DATA_HOME", ""),
                ("XDG_CONFIG_HOME", ""),
            ],
        );
        assert_eq!(l.data, PathBuf::from("/home/me/.local/share/odm"));
        assert_eq!(l.config, PathBuf::from("/home/me/.config/odm"));
    }

    #[test]
    fn with_no_environment_at_all_the_last_resort_is_the_working_directory() {
        let l = layout_of(&UNIX, &[]);
        assert_eq!(l.home, PathBuf::from("."));
        assert_eq!(l.data, PathBuf::from("./.local/share/odm"));
    }

    #[test]
    fn the_platforms_differ_which_is_the_whole_point_of_this_module() {
        // Guards against a future edit collapsing the two specs into one.
        assert_ne!(UNIX.data.from_env, WINDOWS.data.from_env);
        assert_ne!(UNIX.home, WINDOWS.home);
        // Whatever this binary was built for, `layout()` has to agree with its own spec.
        assert_eq!(layout(), layout_for(spec(), &|name| std::env::var_os(name)));
    }

    #[test]
    fn a_short_write_is_retried_until_every_byte_landed() {
        // A writer that accepts at most three bytes per call, as a saturated socket or a
        // filesystem under pressure would.
        let mut landed: Vec<(u64, Vec<u8>)> = Vec::new();
        write_all_at_with(b"hello, world", 100, |part, at| {
            let n = part.len().min(3);
            landed.push((at, part[..n].to_vec()));
            Ok(n)
        })
        .unwrap();

        let mut rebuilt = Vec::new();
        let mut expected_offset = 100u64;
        for (at, chunk) in &landed {
            assert_eq!(*at, expected_offset, "a retry must resume where it stopped");
            expected_offset += chunk.len() as u64;
            rebuilt.extend_from_slice(chunk);
        }
        assert_eq!(rebuilt, b"hello, world");
    }

    #[test]
    fn a_write_of_zero_bytes_is_an_error_rather_than_a_spin() {
        let err = write_all_at_with(b"abc", 0, |_, _| Ok(0)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
    }

    #[test]
    fn a_writer_cannot_claim_more_than_it_was_given() {
        let err = write_all_at_with(b"abc", 0, |part, _| Ok(part.len() + 1)).unwrap_err();
        assert!(err.to_string().contains("more bytes"));
    }

    #[test]
    fn a_real_error_is_returned_without_retrying() {
        let mut calls = 0;
        let err = write_all_at_with(b"abc", 0, |_, _| {
            calls += 1;
            Err(std::io::Error::other("disk on fire"))
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_eq!(err.to_string(), "disk on fire");
    }

    #[test]
    fn an_empty_write_never_calls_the_writer() {
        write_all_at_with(b"", 0, |_, _| panic!("must not be called")).unwrap();
    }

    #[test]
    fn device_names_are_recognised_in_every_spelling() {
        for name in [
            "NUL",
            "nul",
            "Nul",
            "NUL.txt",
            "con",
            "CON.log",
            "PRN",
            "aux",
            "COM1",
            "COM9",
            "lpt1",
            "LPT9.tar.gz",
            "AUX.html",
            "nul.txt.bak",
            "NUL.",
            "COM1 ",
        ] {
            assert!(is_reserved_windows_name(name), "{name} is a device");
        }
        for name in [
            "NULL", "CONX", "COM", "COM0", "COM10", "LPT0", "LPT10", "LPT", "", "download", ".",
            "..", "COM1X",
        ] {
            assert!(
                !is_reserved_windows_name(name),
                "{name} is an ordinary file"
            );
        }
    }
}
