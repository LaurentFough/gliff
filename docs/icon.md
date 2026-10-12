# App icon

Gliff uses the same transparent two-screen icon on Omarchy and macOS. The
1024×1024 RGBA source is [pkgbuild/gliff.png](../pkgbuild/gliff.png). It was
created with image_gen, with the original Omarchy mark placed directly into
the artwork. The exploratory concepts and reference captures have been removed.

The Linux package installs the PNG into hicolor and pixmaps. `bin/install-dev`
uses it for the development launcher and GTK window icon, and `bin/dev` uses
it for build-failure notifications.

The Mac installer includes [Gliff.icns](../crates/gliff-macos/resources/Gliff.icns).
To rebuild that export from the shared PNG on a Mac, run `bin/export-macos-icon`.
It uses Apple's `sips` and `iconutil` for standard and Retina sizes from 16 to
1024 pixels.
