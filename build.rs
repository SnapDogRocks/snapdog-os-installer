// SPDX-License-Identifier: GPL-3.0-only

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        let version = std::env::var("CARGO_PKG_VERSION")
            .expect("Cargo must provide the package version to the Windows resource compiler");
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
        // Keep Explorer's Details tab tied explicitly to the Cargo/Release Please version. The
        // numeric VERSIONINFO fields are initialized from the same Cargo variables by winresource.
        resource.set("FileVersion", &version);
        resource.set("ProductVersion", &version);
        resource.set("ProductName", "SnapDog OS Installer");
        resource.set("FileDescription", "SnapDog OS SD card writer");
        resource.set("CompanyName", "SnapDog");
        resource.set("InternalName", "snapdog-os-installer");
        resource.set("OriginalFilename", "snapdog-os-installer.exe");
        resource.set("LegalCopyright", "Copyright © 2026 Fabian Schmieder");
        resource
            .compile()
            .expect("Windows application resources must compile");
    }
}
