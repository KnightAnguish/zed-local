use anyhow::{Context as _, Result, bail, ensure};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Command,
};

pub const PRODUCT: &str = "Zed Local";
pub const LAUNCHER_EXE: &str = "ZedLocalLauncher.exe";
pub const ZED_EXE: &str = "zedlocal.exe";
pub const DEFAULT_KEEP_BUILDS: usize = 2;

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub build: String,
    pub release: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub source_head: Option<String>,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub product: String,
    pub version: String,
    pub build: String,
    pub release: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub source_head: Option<String>,
    pub installed_at: String,
    pub install_dir: String,
    pub user_data: String,
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub install_root: PathBuf,
    pub user_data: PathBuf,
    pub keep_builds: usize,
    pub shortcuts: bool,
    pub quiet: bool,
}

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub install_root: PathBuf,
    pub build_dir: PathBuf,
    pub user_data: PathBuf,
    pub version: String,
    pub build: String,
    pub release: String,
    pub removed_builds: Vec<PathBuf>,
}

pub fn default_install_root() -> PathBuf {
    local_app_data()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Programs")
        .join("ZedLocal")
}

pub fn default_user_data() -> PathBuf {
    local_app_data()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ZedLocal")
        .join("UserData")
}

fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

pub fn read_install_state(install_root: &Path) -> Result<InstallState> {
    let path = install_root.join("install.json");
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(strip_bom(&bytes)).with_context(|| format!("parse {}", path.display()))
}

pub fn install_from_zip(zip_path: &Path, options: InstallOptions) -> Result<InstallReport> {
    let install_root = absolute(&options.install_root)?;
    let staging = install_root
        .join("updates")
        .join(format!("staging-{}", std::process::id()));
    if staging.exists() {
        fs::remove_dir_all(&staging).with_context(|| format!("clear {}", staging.display()))?;
    }
    fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    let result =
        extract_zip(zip_path, &staging).and_then(|_| install_from_package(&staging, options));
    let _ = fs::remove_dir_all(&staging);
    result
}

pub fn install_from_package(package_dir: &Path, options: InstallOptions) -> Result<InstallReport> {
    ensure!(
        cfg!(target_pointer_width = "64"),
        "Zed Local requires a 64-bit build"
    );
    let install_root = absolute(&options.install_root)?;
    let user_data = absolute(&options.user_data)?;
    let manifest = read_manifest(package_dir)?;
    verify_manifest(package_dir, &manifest)?;

    let build_dir = install_root.join("builds").join(&manifest.build);
    let previous = read_install_state(&install_root).ok().and_then(|state| {
        let path = PathBuf::from(state.install_dir);
        path.is_dir().then_some(path)
    });

    if build_dir.exists() {
        fs::remove_dir_all(&build_dir).with_context(|| {
            format!(
                "Close Zed Local before reinstalling; could not replace {}",
                build_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&build_dir).with_context(|| format!("create {}", build_dir.display()))?;
    fs::create_dir_all(build_dir.join("x64"))?;
    fs::create_dir_all(build_dir.join("bin"))?;
    fs::create_dir_all(&user_data).with_context(|| format!("create {}", user_data.display()))?;

    for file in &manifest.files {
        let relative = safe_relative_path(&file.path)?;
        let source = package_dir.join(&relative);
        let destination = build_dir.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&source, &destination)
            .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    }

    copy_if_exists(
        &package_dir.join("OpenConsole.exe"),
        &build_dir.join("x64").join("OpenConsole.exe"),
    )?;
    copy_if_exists(
        &package_dir.join("cli.exe"),
        &build_dir.join("bin").join("zed.exe"),
    )?;
    for name in ["README.txt", "LICENSE-GPL", "LICENSE-APACHE"] {
        copy_if_exists(&package_dir.join(name), &build_dir.join(name))?;
    }

    let launcher_source = package_dir.join(LAUNCHER_EXE);
    if launcher_source.is_file() {
        let launcher_destination = install_root.join(LAUNCHER_EXE);
        fs::copy(&launcher_source, &launcher_destination).with_context(|| {
            format!(
                "copy {} to {}",
                launcher_source.display(),
                launcher_destination.display()
            )
        })?;
    }

    let state = InstallState {
        product: PRODUCT.to_owned(),
        version: manifest.version.clone(),
        build: manifest.build.clone(),
        release: manifest.release.clone(),
        published_at: manifest.published_at.clone(),
        source_head: manifest.source_head.clone(),
        installed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        install_dir: build_dir.to_string_lossy().into_owned(),
        user_data: user_data.to_string_lossy().into_owned(),
    };
    write_json_atomic(&install_root.join("install.json"), &state)?;

    let removed_builds = prune_builds(
        &install_root,
        &build_dir,
        previous.as_deref(),
        options.keep_builds.max(1),
        &user_data,
    )?;

    if options.shortcuts {
        create_shortcuts(&install_root, &build_dir, &manifest.version)?;
    }

    Ok(InstallReport {
        install_root,
        build_dir,
        user_data,
        version: manifest.version,
        build: manifest.build,
        release: manifest.release,
        removed_builds,
    })
}

pub fn launch(install_root: &Path) -> Result<()> {
    let install_root = absolute(install_root)?;
    let (exe, user_data) = match read_install_state(&install_root) {
        Ok(state) => {
            let exe = PathBuf::from(&state.install_dir).join(ZED_EXE);
            (exe, Some(PathBuf::from(state.user_data)))
        }
        Err(_) => {
            let build_dir = newest_build(&install_root)?;
            (build_dir.join(ZED_EXE), None)
        }
    };
    ensure!(
        exe.is_file(),
        "Zed Local build not found: {}",
        exe.display()
    );
    let mut command = Command::new(&exe);
    command.current_dir(exe.parent().context("build directory is missing")?);
    if let Some(user_data) = user_data {
        command.arg("--user-data-dir").arg(user_data);
    }
    command
        .spawn()
        .with_context(|| format!("start {}", exe.display()))?;
    Ok(())
}

pub fn uninstall(install_root: &Path) -> Result<()> {
    let install_root = absolute(install_root)?;
    remove_shortcuts();
    match fs::remove_dir_all(&install_root) {
        Ok(()) => Ok(()),
        Err(error) => bail!(
            "Could not remove {}: {}. Close Zed Local and delete the folder manually.",
            install_root.display(),
            error
        ),
    }
}

fn read_manifest(package_dir: &Path) -> Result<Manifest> {
    let path = package_dir.join("manifest.json");
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(strip_bom(&bytes)).with_context(|| format!("parse {}", path.display()))
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

fn verify_manifest(package_dir: &Path, manifest: &Manifest) -> Result<()> {
    ensure!(!manifest.build.trim().is_empty(), "manifest build is empty");
    ensure!(!manifest.files.is_empty(), "manifest has no package files");
    for file in &manifest.files {
        let relative = safe_relative_path(&file.path)?;
        let path = package_dir.join(&relative);
        ensure!(path.is_file(), "missing package file: {}", file.path);
        if let Some(size) = file.size {
            let actual = fs::metadata(&path)?.len();
            ensure!(
                actual == size,
                "package size mismatch for {}: expected {}, got {}",
                file.path,
                size,
                actual
            );
        }
        let actual = sha256_file(&path)?;
        ensure!(
            actual.eq_ignore_ascii_case(&file.sha256),
            "package integrity check failed for {}: expected {}, got {}",
            file.path,
            file.sha256,
            actual
        );
    }
    Ok(())
}

fn safe_relative_path(value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    ensure!(!value.is_empty(), "manifest path is empty");
    ensure!(!path.is_absolute(), "absolute manifest path: {value}");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "unsafe manifest path: {value}"
    );
    Ok(path.to_path_buf())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_zip(zip_path: &Path, destination: &Path) -> Result<()> {
    let file = File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("read zip {}", zip_path.display()))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("zip entry has an unsafe path: {}", entry.name());
        };
        let target = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output =
            File::create(&target).with_context(|| format!("create {}", target.display()))?;
        io::copy(&mut entry, &mut output)
            .with_context(|| format!("extract {}", target.display()))?;
    }
    Ok(())
}

fn copy_if_exists(source: &Path, destination: &Path) -> Result<bool> {
    if !source.is_file() {
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)
        .with_context(|| format!("copy {} to {}", source.display(), destination.display()))?;
    Ok(true)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&temporary, bytes).with_context(|| format!("write {}", temporary.display()))?;
    let _ = fs::remove_file(path);
    fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn newest_build(install_root: &Path) -> Result<PathBuf> {
    let builds = install_root.join("builds");
    let mut candidates = fs::read_dir(&builds)
        .with_context(|| format!("read {}", builds.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.join(ZED_EXE).is_file())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| {
        fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
    });
    candidates
        .pop()
        .context("no installed Zed Local build was found")
}

fn prune_builds(
    install_root: &Path,
    current: &Path,
    previous: Option<&Path>,
    keep: usize,
    user_data: &Path,
) -> Result<Vec<PathBuf>> {
    let builds_root = install_root.join("builds");
    if !builds_root.is_dir() {
        return Ok(Vec::new());
    }
    let log_root = user_data
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| install_root.to_path_buf())
        .join("updates");
    let _ = fs::create_dir_all(&log_root);
    let log_path = log_root.join("cleanup.log");

    let mut candidates = fs::read_dir(&builds_root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|path| {
        std::cmp::Reverse(
            fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });

    let mut keep_paths = vec![current.to_path_buf()];
    if let Some(previous) = previous {
        if previous != current {
            keep_paths.push(previous.to_path_buf());
        }
    }

    let mut kept = 0;
    let mut removed = Vec::new();
    for candidate in candidates {
        if keep_paths.iter().any(|path| path == &candidate) {
            kept += 1;
            continue;
        }
        if kept < keep {
            keep_paths.push(candidate);
            kept += 1;
            continue;
        }
        match fs::remove_dir_all(&candidate) {
            Ok(()) => {
                removed.push(candidate.clone());
                append_log(&log_path, &format!("removed {}", candidate.display()));
            }
            Err(error) => {
                append_log(
                    &log_path,
                    &format!("kept {}: {}", candidate.display(), error),
                );
            }
        }
    }
    Ok(removed)
}

fn append_log(path: &Path, message: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{} {}",
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            message
        );
    }
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(windows)]
fn create_shortcuts(install_root: &Path, build_dir: &Path, version: &str) -> Result<()> {
    let launcher = install_root.join(LAUNCHER_EXE);
    if !launcher.is_file() {
        return Ok(());
    }
    let icon = build_dir.join(ZED_EXE);
    let description = format!("{PRODUCT} {version}");
    let folders = [
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Desktop),
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Programs),
    ];
    for folder in folders.into_iter().flatten() {
        let link = folder.join("Zed Local.lnk");
        if let Err(error) =
            create_shortcut_file(&launcher, install_root, &icon, &description, &link)
        {
            log_launcher_error(&format!("create {}: {error:#}", link.display()));
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn create_shortcuts(_install_root: &Path, _build_dir: &Path, _version: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn create_shortcut_file(
    launcher: &Path,
    working_directory: &Path,
    icon: &Path,
    description: &str,
    link: &Path,
) -> Result<()> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{BSTR, HSTRING, Interface};

    if let Some(parent) = link.parent() {
        fs::create_dir_all(parent)?;
    }
    unsafe {
        let initialized = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> Result<()> {
            let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            shell_link.SetPath(&BSTR::from(launcher.to_string_lossy().to_string()))?;
            shell_link.SetWorkingDirectory(&BSTR::from(
                working_directory.to_string_lossy().to_string(),
            ))?;
            shell_link.SetIconLocation(&BSTR::from(format!("{},0", icon.to_string_lossy())), 0)?;
            shell_link.SetDescription(&BSTR::from(description.to_owned()))?;
            let persist: IPersistFile = shell_link.cast()?;
            persist.Save(&HSTRING::from(link.to_string_lossy().to_string()), true)?;
            Ok(())
        })();
        if initialized.is_ok() {
            CoUninitialize();
        }
        result
    }
}

#[cfg(windows)]
fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    unsafe {
        let path = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let value = String::from_utf16_lossy(path.as_wide());
        CoTaskMemFree(Some(path.0 as *const _));
        Some(PathBuf::from(value))
    }
}

#[cfg(windows)]
fn remove_shortcuts() {
    let folders = [
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Desktop),
        known_folder(&windows::Win32::UI::Shell::FOLDERID_Programs),
    ];
    for folder in folders.into_iter().flatten() {
        let _ = fs::remove_file(folder.join("Zed Local.lnk"));
    }
}

#[cfg(not(windows))]
fn remove_shortcuts() {}

#[cfg(windows)]
pub fn show_error(message: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    unsafe {
        let _ = MessageBoxW(
            None,
            &HSTRING::from(message.to_owned()),
            &HSTRING::from("Zed Local"),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
pub fn show_error(message: &str) {
    eprintln!("{message}");
}

fn log_launcher_error(message: &str) {
    let directory = local_app_data()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ZedLocal")
        .join("updates");
    let _ = fs::create_dir_all(&directory);
    append_log(&directory.join("launcher.log"), message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::TempDir;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn package(root: &Path, build: &str) -> Manifest {
        let files = [
            "zedlocal.exe",
            "cli.exe",
            "conpty.dll",
            "OpenConsole.exe",
            "amd_ags_x64.dll",
            "agents/codex-acp/index.js",
            "agents/codex-acp/README.txt",
            "README.txt",
            "LICENSE-GPL",
            "LICENSE-APACHE",
        ];
        let mut manifest_files = Vec::new();
        for (index, name) in files.iter().enumerate() {
            let bytes = format!("{build}:{index}:{name}").into_bytes();
            write(&root.join(name), &bytes);
            manifest_files.push(ManifestFile {
                path: (*name).to_owned(),
                sha256: {
                    let mut hasher = Sha256::new();
                    hasher.update(&bytes);
                    format!("{:x}", hasher.finalize())
                },
                size: Some(bytes.len() as u64),
            });
        }
        write(&root.join(LAUNCHER_EXE), b"launcher");
        let manifest = Manifest {
            version: format!("1.0.0+{build}"),
            build: build.to_owned(),
            release: format!("v1.0.0-{build}"),
            published_at: None,
            source_head: None,
            files: manifest_files,
        };
        write(
            &root.join("manifest.json"),
            &serde_json::to_vec_pretty(&serde_json::json!({
                "version": manifest.version,
                "build": manifest.build,
                "release": manifest.release,
                "files": manifest.files.iter().map(|file| serde_json::json!({
                    "path": file.path,
                    "sha256": file.sha256,
                    "size": file.size,
                })).collect::<Vec<_>>(),
            }))
            .unwrap(),
        );
        manifest
    }

    #[test]
    fn installs_a_package_and_writes_state() {
        let temp = TempDir::new().unwrap();
        let package_dir = temp.path().join("package");
        fs::create_dir_all(&package_dir).unwrap();
        let manifest = package(&package_dir, "test-build");

        let report = install_from_package(
            &package_dir,
            InstallOptions {
                install_root: temp.path().join("install"),
                user_data: temp.path().join("user"),
                keep_builds: 2,
                shortcuts: false,
                quiet: true,
            },
        )
        .unwrap();

        assert_eq!(report.build, manifest.build);
        assert!(report.build_dir.join(ZED_EXE).is_file());
        assert!(report.build_dir.join("bin").join("zed.exe").is_file());
        assert!(
            report
                .build_dir
                .join("x64")
                .join("OpenConsole.exe")
                .is_file()
        );
        assert!(report.install_root.join(LAUNCHER_EXE).is_file());
        assert!(report.user_data.is_dir());
        let state = read_install_state(&report.install_root).unwrap();
        assert_eq!(state.build, manifest.build);
        assert_eq!(PathBuf::from(state.install_dir), report.build_dir);
    }

    #[test]
    fn rejects_a_tampered_package_file() {
        let temp = TempDir::new().unwrap();
        let package_dir = temp.path().join("package");
        fs::create_dir_all(&package_dir).unwrap();
        package(&package_dir, "test-build");
        let mut bytes = fs::read(package_dir.join(ZED_EXE)).unwrap();
        bytes[0] ^= 0xff;
        write(&package_dir.join(ZED_EXE), &bytes);

        let error = install_from_package(
            &package_dir,
            InstallOptions {
                install_root: temp.path().join("install"),
                user_data: temp.path().join("user"),
                keep_builds: 2,
                shortcuts: false,
                quiet: true,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("integrity check failed"));
    }

    #[test]
    fn keeps_the_current_and_previous_builds() {
        let temp = TempDir::new().unwrap();
        let options = InstallOptions {
            install_root: temp.path().join("install"),
            user_data: temp.path().join("user"),
            keep_builds: 2,
            shortcuts: false,
            quiet: true,
        };
        for build in ["build-one", "build-two", "build-three"] {
            let package_dir = temp.path().join(format!("package-{build}"));
            fs::create_dir_all(&package_dir).unwrap();
            package(&package_dir, build);
            install_from_package(&package_dir, options.clone()).unwrap();
        }
        let builds = temp.path().join("install").join("builds");
        assert!(builds.join("build-three").is_dir());
        assert!(builds.join("build-two").is_dir());
        assert!(!builds.join("build-one").exists());
    }

    #[cfg(windows)]
    #[test]
    fn creates_a_shortcut() {
        let temp = TempDir::new().unwrap();
        let launcher = temp.path().join(LAUNCHER_EXE);
        let icon = temp.path().join(ZED_EXE);
        write(&launcher, b"launcher");
        write(&icon, b"icon");
        let link = temp.path().join("test.lnk");
        create_shortcut_file(&launcher, temp.path(), &icon, "Zed Local test", &link).unwrap();
        assert!(link.is_file());
        assert!(fs::metadata(&link).unwrap().len() > 0);
    }
}
