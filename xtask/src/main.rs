//! Build the desktop application normally and its self-contained PDF worker with musl.
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn target() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        _ => Err("The static PDF build supports Linux aarch64 and x86_64 hosts".into()),
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("xtask: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let task = args.next().unwrap_or_default();
    let args: Vec<_> = args.collect();
    if !matches!(task.as_str(), "pdf-worker" | "release")
        || (!args.is_empty() && !(task == "release" && args == ["--features", "video"]))
    {
        return Err(
            "Usage: cargo xtask pdf-worker | cargo xtask release [--features video]".into(),
        );
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    env::set_current_dir(root)?;
    // Respect an explicit Cargo target directory override.
    let directory = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or(root.join("target"));
    fs::create_dir_all(&directory)?;
    let directory = directory.canonicalize()?;
    let target = target()?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    checked(Command::new(&cargo).args([
        "build",
        "--locked",
        "--release",
        "-p",
        "kimbap-pdf-worker",
        "--target",
        target,
    ]))?;
    let binary = directory.join(target).join("release/kimbap-pdf-worker");
    verify_static(&binary)?;
    let release = directory.join("release");
    fs::create_dir_all(&release)?;
    let staged = release.join(".kimbap-pdf-worker");
    fs::copy(&binary, &staged)?;
    fs::rename(staged, release.join("kimbap-pdf-worker"))?;
    let notices = release.join("pdf-licenses");
    fs::create_dir_all(&notices)?;
    fs::copy(
        root.join("data/pdf-notices.txt"),
        notices.join("third-party.txt"),
    )?;
    if task == "release" {
        checked(
            Command::new(&cargo)
                .args([
                    "build",
                    "--locked",
                    "--release",
                    "-p",
                    "kimbap",
                    "-p",
                    "kimbap-jpeg-worker",
                ])
                .args(args),
        )?;
    }
    println!(
        "Static musl PDF worker: {}",
        release.join("kimbap-pdf-worker").display()
    );
    Ok(())
}
fn checked(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        return Err(format!("{command:?} failed with {status}").into());
    }
    Ok(())
}
fn verify_static(path: &Path) -> Result<()> {
    let output = Command::new("readelf")
        .args(["--program-headers", "--dynamic", "--wide"])
        .arg(path)
        .env("LC_ALL", "C")
        .output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || text.contains("INTERP")
        || text.contains("(NEEDED)")
        || text.contains("(RPATH)")
        || text.contains("(RUNPATH)")
    {
        return Err(format!(
            "{} is not a self-contained static executable",
            path.display()
        )
        .into());
    }
    println!("Verified: no ELF interpreter or shared-library dependencies");
    Ok(())
}
