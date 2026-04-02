use std::fs;
use std::path::PathBuf;
use std::process::Command;

use colored::Colorize;

#[cfg(unix)]
fn wrapper_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/bin/sem-diff-wrapper")
}

#[cfg(windows)]
fn wrapper_path() -> PathBuf {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\Default".to_string());
    PathBuf::from(home).join(".local\\bin\\sem-diff-wrapper.bat")
}

#[cfg(unix)]
fn wrapper_script() -> String {
    "#!/bin/sh\n\
     # Wrapper for git diff.external: translates git's 7-arg format to sem diff\n\
     # Args: path old-file old-hex old-mode new-file new-hex new-mode\n\
     exec sem diff \"$2\" \"$5\"\n"
        .to_string()
}

#[cfg(windows)]
fn wrapper_script() -> String {
    "@echo off\r\n\
     rem Wrapper for git diff.external: translates git's 7-arg format to sem diff\r\n\
     rem Args: path old-file old-hex old-mode new-file new-hex new-mode\r\n\
     sem diff \"%~2\" \"%~5\"\r\n"
        .to_string()
}

#[cfg(unix)]
fn wrapper_name() -> &'static str {
    "sem-diff-wrapper"
}

#[cfg(windows)]
fn wrapper_name() -> &'static str {
    "sem-diff-wrapper.bat"
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(windows)]
fn set_executable(_path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    // .bat files are executable by default on Windows
    Ok(())
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = wrapper_path();
    let dir = path.parent().unwrap();

    // Create wrapper directory if needed
    if !dir.exists() {
        fs::create_dir_all(dir)?;
        println!(
            "{} Created {}",
            "✓".green().bold(),
            dir.display()
        );
    }

    // Write wrapper script
    fs::write(&path, wrapper_script())?;
    set_executable(&path)?;
    println!(
        "{} Created wrapper script at {}",
        "✓".green().bold(),
        path.display()
    );

    // Set diff.external globally
    let status = Command::new("git")
        .args(["config", "--global", "diff.external", wrapper_name()])
        .status()?;
    if !status.success() {
        return Err("Failed to set diff.external in git config".into());
    }
    println!(
        "{} Set git config --global diff.external = {}",
        "✓".green().bold(),
        wrapper_name(),
    );

    println!(
        "\n{} Running `git diff` in any repo will now use sem.",
        "Done!".green().bold()
    );
    println!("To revert, run: sem unsetup");

    Ok(())
}

pub fn unsetup() -> Result<(), Box<dyn std::error::Error>> {
    // Unset diff.external
    let status = Command::new("git")
        .args(["config", "--global", "--unset", "diff.external"])
        .status()?;
    if status.success() {
        println!(
            "{} Removed diff.external from global git config",
            "✓".green().bold(),
        );
    } else {
        println!(
            "{} diff.external was not set in global git config",
            "✓".green().bold(),
        );
    }

    // Remove wrapper script
    let path = wrapper_path();
    if path.exists() {
        fs::remove_file(&path)?;
        println!(
            "{} Removed wrapper script at {}",
            "✓".green().bold(),
            path.display()
        );
    }

    println!(
        "\n{} git diff restored to default behavior.",
        "Done!".green().bold()
    );

    Ok(())
}
