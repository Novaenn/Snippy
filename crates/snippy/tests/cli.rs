use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "snippy-cli-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create isolated CLI test directory");
        Self(path)
    }

    fn run(&self, args: &[&str]) -> Output {
        let output = self.run_with_state(args);
        assert_eq!(fs::read_dir(&self.0).unwrap().count(), 0);
        output
    }

    fn run_with_state(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_snippy"))
            .args(args)
            .current_dir(&self.0)
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .output()
            .expect("run snippy")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn help_and_version_work_without_a_display_or_service() {
    let sandbox = Sandbox::new();
    let help = sandbox.run(&["--help"]);
    assert!(help.status.success());
    let text = String::from_utf8(help.stdout).unwrap();
    for command in ["daemon", "save", "picker", "manager", "lock", "status"] {
        assert!(text.contains(command));
    }
    assert!(text.contains("Picker"));
    let version = sandbox.run(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap().trim(),
        concat!("snippy ", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_arguments_and_missing_command_are_usage_errors() {
    let sandbox = Sandbox::new();
    for args in [
        vec![],
        vec!["unknown-command"],
        vec!["save", "--unexpected"],
        vec!["save", "extra-argument"],
    ] {
        let result = sandbox.run(&args);
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        assert!(!result.stderr.is_empty());
    }
}

#[test]
fn service_commands_fail_clearly_without_a_service_and_create_no_directories() {
    let sandbox = Sandbox::new();
    for action in ["manager", "lock", "status", "save", "picker"] {
        let result = sandbox.run(&[action]);
        assert_eq!(result.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&result.stderr).contains("service unavailable"));
    }
}

#[test]
fn daemon_requires_display_before_creating_runtime_files() {
    let sandbox = Sandbox::new();
    let result = sandbox.run(&["daemon"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("X11"));
}

#[test]
fn invalid_settings_fail_before_creating_runtime_or_vault_state() {
    let sandbox = Sandbox::new();
    let config = sandbox.0.join("config/snippy");
    fs::create_dir_all(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
    let settings = config.join("settings.conf");
    fs::write(&settings, "max_clip_bytes=0\n").unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    let result = sandbox.run_with_state(&["daemon", "--headless"]);
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("settings file is invalid"),
        "unexpected diagnostic: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        !sandbox.0.join("data").exists(),
        "invalid settings created vault data"
    );
    assert!(
        !sandbox.0.join("runtime").exists(),
        "invalid settings created runtime state"
    );
    assert_eq!(fs::read_to_string(&settings).unwrap(), "max_clip_bytes=0\n");
}

#[test]
fn invalid_shortcut_overrides_fail_startup_without_creating_state() {
    let sandbox = Sandbox::new();
    let long = "x".repeat(65);
    for args in [
        ["daemon", "--headless", "--save-shortcut", long.as_str()],
        ["daemon", "--headless", "--picker-shortcut", "Ctrl+\u{1b}V"],
    ] {
        let result = sandbox.run(&args);
        assert_eq!(result.status.code(), Some(1));
        assert!(
            String::from_utf8_lossy(&result.stderr).contains("invalid"),
            "unexpected diagnostic: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

#[test]
fn opt_in_autostart_is_created_at_startup() {
    let sandbox = Sandbox::new();
    let config = sandbox.0.join("config/snippy");
    fs::create_dir_all(&config).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o700)).unwrap();
    let settings = config.join("settings.conf");
    fs::write(&settings, "autostart=true\n").unwrap();
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o600)).unwrap();
    // Startup takes the single-instance lock before touching autostart, so it
    // needs a valid private runtime directory.
    let runtime = sandbox.0.join("runtime");
    fs::create_dir(&runtime).unwrap();
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_snippy"))
        .args(["daemon", "--headless"])
        .current_dir(&sandbox.0)
        .env("XDG_DATA_HOME", sandbox.0.join("data"))
        .env("XDG_CONFIG_HOME", sandbox.0.join("config"))
        .env("XDG_RUNTIME_DIR", sandbox.0.join("runtime"))
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let desktop = sandbox.0.join("config/autostart/snippy.desktop");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !desktop.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    let text = fs::read_to_string(&desktop).expect("opt-in autostart entry");
    assert!(text.contains("Name=snippy"));
    assert!(text.contains(" daemon\n"));
    assert!(text.contains("X-snippy-Autostart=true"));
    assert_eq!(
        fs::metadata(&desktop).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
