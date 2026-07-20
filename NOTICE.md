# Asset provenance

The SnapDog SD-card application icon, Raspberry Pi illustrations, SnapDog logo, and DMG background
were transferred from `SnapDogRocks/snapdog-etcher` for the dedicated Rust implementation. The
assets are maintained in this repository so that builds do not depend on another checkout.

`vendor/wayland-scanner` contains the MIT-licensed `wayland-scanner` 0.31.10 release source. Its
manifest carries the upstream `quick-xml` 0.41 security dependency update while preserving the
released scanner API required by the current Linux GUI stack. The original license is included in
that directory.
