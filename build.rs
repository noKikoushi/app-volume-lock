// Embeds the icon, the manifest (DPI awareness) and the version info into the exe.
fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/app.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");
    embed_resource::compile("assets/app.rc", embed_resource::NONE);
}
