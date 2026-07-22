use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    let command = env::args().nth(1).unwrap_or_else(|| "help".into());
    match command.as_str() {
        "pi-build" => pi_build(),
        "pi-dev" => pi_dev(),
        "app" => run("cargo", &["run", "-p", "pi-whim-app"], &workspace_root()),
        "package-macos" => package_macos(),
        "smoke" => smoke(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => bail!("unknown xtask command: {command}"),
    }
}

fn print_help() {
    println!(
        "Pi-Whim automation\n\n  cargo run -p xtask -- pi-build\n  cargo run -p xtask -- pi-dev\n  cargo run -p xtask -- app\n  cargo run -p xtask -- package-macos\n  cargo run -p xtask -- smoke"
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}
fn pi_root() -> PathBuf {
    workspace_root().join("vendor/pi-mono")
}

fn run(program: &str, args: &[&str], directory: &Path) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .current_dir(directory)
        .status()
        .with_context(|| format!("failed to launch {program}"))?;
    if status.success() {
        Ok(())
    } else {
        bail!("{program} exited with {status}")
    }
}

fn pi_build() -> Result<()> {
    let root = pi_root();
    if !root.join("package.json").is_file() {
        bail!("Pi source is absent; run `git submodule update --init --recursive`");
    }
    run("npm", &["ci", "--ignore-scripts"], &root)?;
    run("npm", &["run", "build"], &root)?;
    run(
        "./scripts/build-binaries.sh",
        &["--platform", "darwin-arm64"],
        &root,
    )
}

fn pi_dev() -> Result<()> {
    let root = pi_root();
    let binary = root.join("packages/coding-agent/binaries/darwin-arm64/pi");
    if !binary.is_file() {
        pi_build()?;
    }
    let status = Command::new("cargo")
        .args(["run", "-p", "pi-whim-app"])
        .current_dir(workspace_root())
        .env("PI_WHIM_PI_BIN", binary)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("application exited with {status}")
    }
}

fn package_macos() -> Result<()> {
    let root = workspace_root();
    let pi_resources = pi_root().join("packages/coding-agent/binaries/darwin-arm64");
    if !pi_resources.join("pi").is_file() {
        pi_build()?;
    }
    run("cargo", &["build", "--release", "-p", "pi-whim-app"], &root)?;
    let app = root.join("dist/Pi-Whim.app");
    if app.exists() {
        fs::remove_dir_all(&app)?;
    }
    let macos = app.join("Contents/MacOS");
    let resources = app.join("Contents/Resources/pi");
    fs::create_dir_all(&macos)?;
    fs::copy(root.join("target/release/pi-whim"), macos.join("Pi-Whim"))?;
    copy_directory(&pi_resources, &resources)?;
    let plist = r#"<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\"><dict><key>CFBundleExecutable</key><string>Pi-Whim</string><key>CFBundleIdentifier</key><string>dev.pi-whim.desktop</string><key>CFBundleName</key><string>Pi-Whim</string><key>CFBundlePackageType</key><string>APPL</string><key>CFBundleShortVersionString</key><string>0.1.0</string><key>LSMinimumSystemVersion</key><string>13.0</string></dict></plist>"#;
    fs::write(app.join("Contents/Info.plist"), plist)?;
    println!("Built {}", app.display());
    Ok(())
}

fn copy_directory(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_directory(&source_path, &destination_path)?;
        } else {
            fs::copy(&source_path, &destination_path)?;
        }
    }
    Ok(())
}

fn smoke() -> Result<()> {
    if env::var_os("PI_WHIM_SMOKE").is_none() {
        println!(
            "Skipping real Pi smoke test; set PI_WHIM_SMOKE=1 and configure a provider key to enable it."
        );
        return Ok(());
    }
    let root = workspace_root();
    let executable = env::var("PI_WHIM_PI_BIN")
        .context("PI_WHIM_PI_BIN must point to the packaged Pi executable")?;
    let status = Command::new(executable)
        .args(["--mode", "rpc", "--no-session"])
        .current_dir(root)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("Pi smoke command exited with {status}")
    }
}
