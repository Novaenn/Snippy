//! Opt-in XDG desktop autostart entry. No autostart is created unless enabled.

use crate::ipc::Error;
use std::{
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MARKER: &str = "X-snippy-Autostart=true";
const MAX_EXISTING_BYTES: u64 = 16 * 1024;

#[derive(Clone)]
pub struct Autostart {
    path: PathBuf,
}

impl Autostart {
    pub fn discover() -> Result<Self, Error> {
        let root = match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
            Some(root) => PathBuf::from(root),
            None => {
                PathBuf::from(std::env::var_os("HOME").ok_or(Error::UnsafePath)?).join(".config")
            }
        };
        if !root.is_absolute() {
            return Err(Error::UnsafePath);
        }
        Ok(Self {
            path: root.join("autostart").join("snippy.desktop"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn set(&self, enabled: bool, executable: &Path) -> Result<(), Error> {
        let directory = self.path.parent().ok_or(Error::UnsafePath)?;
        if enabled {
            if !executable.is_absolute() {
                return Err(Error::Autostart);
            }
            self.ensure_directory(directory)?;
            self.ensure_replaceable()?;
            self.write(&self.render(executable)?)
        } else {
            if !directory.exists() {
                return Ok(());
            }
            self.ensure_directory(directory)?;
            match fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(Error::Autostart),
                Ok(metadata) => {
                    if metadata.file_type().is_symlink()
                        || !metadata.is_file()
                        || metadata.uid() != rustix::process::geteuid().as_raw()
                    {
                        return Err(Error::UnsafePath);
                    }
                    self.ensure_ours()?;
                    fs::remove_file(&self.path).map_err(|_| Error::Autostart)
                }
            }
        }
    }

    fn ensure_directory(&self, directory: &Path) -> Result<(), Error> {
        match DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
        {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(_) => return Err(Error::Autostart),
        }
        let metadata = fs::symlink_metadata(directory).map_err(|_| Error::Autostart)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            return Err(Error::UnsafePath);
        }
        Ok(())
    }

    fn ensure_replaceable(&self) -> Result<(), Error> {
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Error::Autostart),
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.uid() != rustix::process::geteuid().as_raw()
                {
                    return Err(Error::UnsafePath);
                }
                self.ensure_ours()
            }
        }
    }

    fn ensure_ours(&self) -> Result<(), Error> {
        let metadata = fs::symlink_metadata(&self.path).map_err(|_| Error::Autostart)?;
        if metadata.len() > MAX_EXISTING_BYTES {
            return Err(Error::Autostart);
        }
        let text = fs::read_to_string(&self.path).map_err(|_| Error::Autostart)?;
        if text.lines().any(|line| line.trim() == MARKER) {
            Ok(())
        } else {
            // Do not delete or overwrite a desktop file we did not create.
            Err(Error::Autostart)
        }
    }

    fn render(&self, executable: &Path) -> Result<String, Error> {
        let quoted = exec_argument(executable)?;
        Ok(format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=snippy\n\
             Comment=Explicit-save clipboard manager; starts locked\n\
             Exec={quoted} daemon\n\
             Terminal=false\n\
             NoDisplay=false\n\
             X-GNOME-Autostart-enabled=true\n\
             {MARKER}\n"
        ))
    }

    fn write(&self, contents: &str) -> Result<(), Error> {
        let directory = self.path.parent().ok_or(Error::UnsafePath)?;
        let temporary = directory.join(format!(".snippy.desktop.{}.tmp", std::process::id()));
        let _ = fs::remove_file(&temporary);
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(&temporary)
                .map_err(|_| Error::Autostart)?;
            file.write_all(contents.as_bytes())
                .and_then(|()| file.sync_all())
                .map_err(|_| Error::Autostart)?;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|_| Error::Autostart)?;
            fs::rename(&temporary, &self.path).map_err(|_| Error::Autostart)?;
            if let Ok(directory) = fs::File::open(directory) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

/// Quote one Exec argument per the Desktop Entry spec. The value is first
/// string-unescaped (`\\` becomes `\`), then `"`, `` ` ``, `$`, and `\` inside
/// double quotes need a backslash, so the file holds a doubled backslash
/// before each. `%` is doubled because it introduces field codes.
fn exec_argument(executable: &Path) -> Result<String, Error> {
    let executable = executable.to_str().ok_or(Error::Autostart)?;
    if executable.chars().any(char::is_control) {
        return Err(Error::Autostart);
    }
    let mut quoted = String::with_capacity(executable.len() + 8);
    quoted.push('"');
    for character in executable.chars() {
        match character {
            '"' | '`' | '$' => {
                quoted.push_str("\\\\");
                quoted.push(character);
            }
            '\\' => quoted.push_str("\\\\\\\\"),
            '%' => quoted.push_str("%%"),
            _ => quoted.push(character),
        }
    }
    quoted.push('"');
    Ok(quoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable_and_foreign_files_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("autostart").join("snippy.desktop");
        let autostart = Autostart::at(&file);
        let executable = Path::new("/opt/clip mgr%$`\"\\bin/snippy");
        autostart.set(true, executable).unwrap();
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains(MARKER));
        assert!(text.contains("Exec="));
        assert!(text.contains("%%"));
        assert!(text.contains(" daemon\n"));
        assert_eq!(fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(file.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        autostart.set(false, executable).unwrap();
        assert!(!file.exists());
        autostart.set(false, executable).unwrap();

        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "[Desktop Entry]\nName=someone-else\n").unwrap();
        assert_eq!(autostart.set(false, executable), Err(Error::Autostart));
        assert!(file.exists());
        assert_eq!(autostart.set(true, executable), Err(Error::Autostart));
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "[Desktop Entry]\nName=someone-else\n"
        );

        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink("/tmp/foreign", &file).unwrap();
        assert_eq!(autostart.set(false, executable), Err(Error::UnsafePath));
    }

    #[test]
    fn exec_line_follows_desktop_entry_escaping() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("autostart").join("snippy.desktop");
        let autostart = Autostart::at(&file);
        autostart
            .set(true, Path::new("/opt/clip mgr$\"\\`50%/snippy"))
            .unwrap();
        let text = fs::read_to_string(&file).unwrap();
        let exec = text.lines().find(|l| l.starts_with("Exec=")).unwrap();
        assert_eq!(
            exec,
            r#"Exec="/opt/clip mgr\\$\\"\\\\\\`50%%/snippy" daemon"#
        );
    }

    #[test]
    fn non_utf8_and_control_character_paths_are_rejected() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("autostart").join("snippy.desktop");
        let autostart = Autostart::at(&file);
        for path in [
            Path::new(OsStr::from_bytes(b"/opt/clip\xffmgr")),
            Path::new("/opt/clip\nmgr"),
            Path::new("/opt/clip\tmgr"),
        ] {
            assert_eq!(autostart.set(true, path), Err(Error::Autostart));
        }
        assert!(!file.exists());
    }
}
