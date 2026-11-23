//! The installed command is `vgpu`; the package stays `player-app`.
//!
//! Cargo exposes the built binary to integration tests as
//! `CARGO_BIN_EXE_vgpu`, so this also catches an accidental rename that would
//! break PATH, shortcuts, and file associations.

#[test]
fn release_binary_is_named_vgpu() {
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_vgpu"));

    assert_eq!(
        path.file_stem().and_then(|stem| stem.to_str()),
        Some("vgpu")
    );
}
