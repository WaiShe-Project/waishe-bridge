//! Ensures `dashboard-dist/` exists so the embedded assets compile without a Dashboard build.
//! Release builds copy `waishe/dashboard/dist` here first (see README).

use std::path::Path;

fn main() {
    let dist = Path::new("dashboard-dist");
    println!("cargo:rerun-if-changed=dashboard-dist");
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(dist).expect("create dashboard-dist");
        std::fs::write(
            dist.join("index.html"),
            "<!doctype html><meta charset=utf-8><title>WaiShe</title>\
             <p>This Bridge was built without the Dashboard. Copy waishe/dashboard/dist into \
             waishe-bridge/dashboard-dist and rebuild.</p>",
        )
        .expect("write placeholder");
    }
}
