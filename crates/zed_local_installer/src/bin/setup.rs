use anyhow::Context as _;
use std::io::{self, Write as _};
use std::path::PathBuf;
use zed_local_installer::{
    InstallOptions, default_install_root, default_user_data, install_from_package,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("Zed Local setup failed: {error:#}");
        eprintln!();
        eprintln!("Press Enter to close.");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }

    let package_dir = std::env::current_exe()?
        .parent()
        .map(PathBuf::from)
        .context("setup executable has no directory")?;
    let install_root = value(&args, "--install-root")
        .map(PathBuf::from)
        .unwrap_or_else(default_install_root);
    let user_data = value(&args, "--user-data")
        .map(PathBuf::from)
        .unwrap_or_else(default_user_data);
    let keep_builds = value(&args, "--keep-builds")
        .and_then(|value| value.parse().ok())
        .unwrap_or(zed_local_installer::DEFAULT_KEEP_BUILDS);
    let shortcuts = !args.iter().any(|arg| arg == "--no-shortcuts");
    let quiet = args.iter().any(|arg| arg == "--quiet");

    if !quiet {
        println!("Installing Zed Local from {}", package_dir.display());
    }
    let report = install_from_package(
        &package_dir,
        InstallOptions {
            install_root,
            user_data,
            keep_builds,
            shortcuts,
            quiet,
        },
    )?;

    if !quiet {
        println!();
        println!("Zed Local installed successfully.");
        if report.reused_running_build {
            println!(
                "Zed Local is running; its build files were kept and the launcher/shortcuts were refreshed."
            );
            println!("Close Zed Local and run Install.exe again to replace the build files.");
        }
        println!("Application: {}", report.build_dir.display());
        println!("User data:   {}", report.user_data.display());
        if shortcuts {
            println!("Shortcuts:   Desktop and Start Menu -> Zed Local");
        }
        println!(
            "Start:       ZedLocalLauncher.exe in {}",
            report.install_root.display()
        );
        println!();
        println!("Press Enter to close.");
        let _ = io::stdout().flush();
        let _ = io::stdin().read_line(&mut String::new());
    }
    Ok(())
}

fn value(args: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    args.iter()
        .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_owned))
        .or_else(|| {
            args.iter()
                .position(|arg| arg == name)
                .and_then(|index| args.get(index + 1))
                .cloned()
        })
}

fn print_help() {
    println!(
        "Zed Local setup\n\n\
         Usage: Install.exe [--install-root PATH] [--user-data PATH] [--keep-builds N]\n\
         \x20                 [--no-shortcuts] [--quiet]\n\n\
         The package directory must contain manifest.json and the Zed Local build files."
    );
}
