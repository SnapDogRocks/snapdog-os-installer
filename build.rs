// SPDX-License-Identifier: GPL-3.0-only

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set("ProductName", "SnapDog OS Installer");
        resource.set("FileDescription", "SnapDog OS SD card writer");
        resource
            .compile()
            .expect("Windows application resources must compile");
    }
}
