#!/bin/sh
set -eu

export RUSTFLAGS="-Dwarnings"

cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features
cargo +1.88.0 check --locked --all-targets
cargo deny check
cargo audit

NOTICES=$(mktemp)
trap 'rm -f "$NOTICES"' EXIT
./scripts/generate-notices.sh "$NOTICES"
if ! cmp -s THIRD_PARTY_NOTICES.txt "$NOTICES"; then
  echo "THIRD_PARTY_NOTICES.txt is stale; run ./scripts/generate-notices.sh" >&2
  exit 1
fi

shellcheck scripts/*.sh packaging/linux/*.sh
actionlint
if command -v desktop-file-validate >/dev/null 2>&1 &&
  command -v appstreamcli >/dev/null 2>&1; then
  desktop-file-validate packaging/linux/cc.snapdog.os-installer.desktop
  appstreamcli validate --no-net packaging/linux/cc.snapdog.os-installer.appdata.xml
elif [ "$(uname -s)" = Linux ]; then
  echo "desktop-file-validate and appstreamcli are required on Linux" >&2
  exit 1
else
  echo "Skipping Linux desktop metadata validators on $(uname -s); CI enforces them on Linux"
fi

# Parse the Windows packager without executing it.
# shellcheck disable=SC2016
pwsh -NoLogo -NoProfile -NonInteractive -Command '
  $tokens = $null
  $errors = $null
  [System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path "scripts/package-windows.ps1"),
    [ref]$tokens,
    [ref]$errors
  ) | Out-Null
  if ($errors.Count -ne 0) {
    $errors | ForEach-Object { Write-Error $_ }
    exit 1
  }
'

./scripts/check-live-manifests.py --self-test

python3 - <<'PY'
import json
import plistlib
import re
import struct
import tomllib
import xml.etree.ElementTree as ET
from pathlib import Path

root = Path.cwd()

release_config = json.loads((root / "release-please-config.json").read_text())
release_manifest = json.loads(
    (root / ".release-please-manifest.json").read_text()
)
release_package = release_config["packages"]["."]
assert release_config["bootstrap-sha"] == (
    "f49f0fb9f78da3b7f5aae61f8b2f0b7931508a85"
)
assert release_package["release-type"] == "rust"
assert release_package["initial-version"] == "0.1.0"
assert release_package["draft"] is True
assert release_package["force-tag-creation"] is True
assert release_package["include-component-in-tag"] is False
assert release_package["include-v-in-tag"] is True
assert release_manifest == {".": "0.0.0"} or release_manifest == {
    ".": tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
}

release_please_workflow = (
    root / ".github/workflows/release-please.yml"
).read_text()
assert "secrets.TAP_TOKEN" in release_please_workflow
assert "--auto" not in release_please_workflow
assert "pull_request_target" not in release_please_workflow

release_workflow = (root / ".github/workflows/release.yml").read_text()
assert 'gh release upload "$GITHUB_REF_NAME"' in release_workflow
assert 'gh release edit "$GITHUB_REF_NAME"' in release_workflow
assert "--draft=false" in release_workflow

macos_pipeline = (root / "src/pipeline/macos.rs").read_text()
runtime_requirement = re.search(
    r'const WORKER_REQUIREMENT: &str = r#"(.*?)"#;', macos_pipeline
)
assert runtime_requirement is not None
macos_packager = (root / "scripts/package-macos.sh").read_text()
packaging_requirement = re.search(
    r"^WORKER_REQUIREMENT='(.*)'$", macos_packager, re.MULTILINE
)
assert packaging_requirement is not None
assert runtime_requirement.group(1) == packaging_requirement.group(1)
assert 'Command::new("/usr/bin/osascript")' not in macos_pipeline
assert "ELEVATION_SCRIPT" not in macos_pipeline
assert "ELEVATED_SHELL_PROGRAM" not in macos_pipeline
assert ".env(RAW_DEVICE_OPT_IN, RAW_DEVICE_OPT_IN_VALUE)" in macos_pipeline
assert "Command::new(executable)" in macos_pipeline
assert '.arg(AUTHOPEN_RAW_FLAGS)' in macos_pipeline
assert '.arg("--requirement")' not in macos_pipeline
assert '"-R=$WORKER_REQUIREMENT"' in macos_packager
assert "umask 077" in macos_packager
assert "UNVALIDATED_DMG" in macos_packager
assert 'mv -f "$UNVALIDATED_DMG" "$DMG"' in macos_packager

build_script = (root / "build.rs").read_text()
assert 'CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")' in build_script
assert 'requestedExecutionLevel level=\"asInvoker\"' in build_script
for version_field in ("FileVersion", "ProductVersion"):
    assert f'resource.set("{version_field}", &version)' in build_script
for metadata_value in (
    "SnapDog OS Installer",
    "snapdog-os-installer.exe",
    "Copyright © 2026 Fabian Schmieder",
):
    assert metadata_value in build_script

windows_packager = (root / "scripts/package-windows.ps1").read_text()
static_windows_flags = "-Dwarnings -C target-feature=+crt-static"
assert static_windows_flags in windows_packager
assert "dumpbin.exe" in windows_packager
assert "$TemporaryOutput" in windows_packager
assert "Move-Item -Force -LiteralPath $TemporaryOutput -Destination $Output" in windows_packager
assert "$VersionInfo.FileVersion -ne $Package.version" in windows_packager
assert "$VersionInfo.ProductVersion -ne $Package.version" in windows_packager
for runtime_name in ("vcruntime", "msvcp", "msvcr", "concrt", "ucrtbase", "api-ms-win-crt-"):
    assert runtime_name in windows_packager.lower()

windows_runtime_paths = (
    "src/drives.rs",
    "src/pipeline/windows.rs",
    "src/worker.rs",
    "src/worker/windows.rs",
    "src/windows_native.rs",
)
windows_runtime = "\n".join((root / path).read_text() for path in windows_runtime_paths)
for forbidden in ("powershell", "start-process", "get-disk", "set-disk"):
    assert forbidden not in windows_runtime.lower()
for required in (
    "ShellExecuteExW",
    "FSCTL_LOCK_VOLUME",
    "FSCTL_DISMOUNT_VOLUME",
    "IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS",
):
    assert required in windows_runtime

assert static_windows_flags in release_workflow

info = plistlib.loads((root / "packaging/macos/Info.plist").read_bytes())
assert info["CFBundleIdentifier"] == "cc.snapdog.os-installer"
assert info["CFBundleExecutable"] == "snapdog-os-installer"
assert info["CFBundleShortVersionString"] == "__VERSION__"
assert info["NSRemovableVolumesUsageDescription"] == (
    "SnapDog OS Installer needs access to removable volumes to write and verify "
    "SnapDog OS on the selected SD card."
)
assert info["CFBundleVersion"] == "__VERSION__"

entitlements = plistlib.loads(
    (root / "packaging/macos/entitlements.plist").read_bytes()
)
assert entitlements == {}

component = ET.parse(
    root / "packaging/linux/cc.snapdog.os-installer.appdata.xml"
).getroot()
assert component.tag == "component"
assert component.findtext("id") == "cc.snapdog.os-installer"

def png_dimensions(path: Path) -> tuple[int, int]:
    data = path.read_bytes()
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    return struct.unpack(">II", data[16:24])


assert png_dimensions(root / "packaging/linux/cc.snapdog.os-installer.png") == (
    512,
    512,
)
assert png_dimensions(root / "assets/icon.png") == (1024, 1024)
assert png_dimensions(root / "assets/icon-windows.png") == (1024, 1024)
assert png_dimensions(root / "assets/icon-macos.png") == (1024, 1024)
assert png_dimensions(root / "assets/dmg/background.png") == (600, 400)
assert (root / "assets/icon.icns").read_bytes()[:4] == b"icns"
assert (root / "assets/icon.ico").read_bytes()[:4] == b"\x00\x00\x01\x00"

main_source = (root / "src/main.rs").read_text()
assert '#[cfg(target_os = "windows")]' in main_source
assert 'include_bytes!("../assets/icon-windows.png")' in main_source
PY
