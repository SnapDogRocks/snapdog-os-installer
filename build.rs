// SPDX-License-Identifier: GPL-3.0-only

fn main() {
    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        resource.set("ProductName", "SnapDog OS Installer");
        resource.set("FileDescription", "SnapDog OS SD card writer");
        resource
            .compile()
            .expect("Windows application resources must compile");
    }
}
