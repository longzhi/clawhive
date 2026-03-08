use std::path::Path;
use std::process::Command;

fn main() {
    // Re-run only when dist/index.html changes or is missing
    println!("cargo:rerun-if-changed=../../web/dist/index.html");

    let dist_index = Path::new("../../web/dist/index.html");
    if dist_index.exists() {
        return;
    }

    eprintln!("Frontend not built. Running `bun run build` in web/ ...");

    let web_dir = Path::new("../../web");

    // Install dependencies if needed
    if !web_dir.join("node_modules").exists() {
        let install = Command::new("bun")
            .arg("install")
            .current_dir(web_dir)
            .status();

        match install {
            Ok(s) if s.success() => {}
            _ => {
                panic!(
                    "\n\nFrontend dependencies not installed and `bun install` failed.\n\
                     Please run manually:\n\n  cd web && bun install && bun run build\n\n"
                );
            }
        }
    }

    // Build frontend
    let build = Command::new("bun")
        .args(["run", "build"])
        .current_dir(web_dir)
        .status();

    match build {
        Ok(s) if s.success() => {
            eprintln!("Frontend built successfully.");
        }
        _ => {
            panic!(
                "\n\nFrontend build failed. `bun run build` returned an error.\n\
                 Please run manually:\n\n  cd web && bun install && bun run build\n\n"
            );
        }
    }
}
