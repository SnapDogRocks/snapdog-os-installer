// SPDX-License-Identifier: GPL-3.0-only

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/icon.ico");
        // Explicitly opt out of Windows' filename-based legacy installer detection. The GUI must
        // run as the desktop user; only the digest-bound raw-device worker crosses UAC through
        // ShellExecuteExW when the user starts flashing.
        resource.set_manifest(
            r#"<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false" />
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>"#,
        );
        resource.set("ProductName", "SnapDog OS Installer");
        resource.set("FileDescription", "SnapDog OS SD card writer");
        resource
            .compile()
            .expect("Windows application resources must compile");
    }
}
