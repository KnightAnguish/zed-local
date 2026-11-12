#![windows_subsystem = "windows"]

use std::path::PathBuf;
use zed_local_installer::{default_install_root, launch, repair_shortcuts, show_error, uninstall};

fn main() {
    let install_root = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(default_install_root);
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--repair-shortcuts") {
        if let Err(error) = repair_shortcuts(&install_root) {
            show_error(&format!(
                "Could not repair the Zed Local shortcuts.\n\n{error:#}"
            ));
        }
        return;
    }

    if args.iter().any(|arg| arg == "--uninstall") {
        if let Err(error) = uninstall(&install_root) {
            show_error(&format!("{error:#}"));
        }
        return;
    }

    if let Err(error) = launch(&install_root) {
        show_error(&format!("Could not start Zed Local.\n\n{error:#}"));
    }
}
