//! `cargo run -p uniffi-bindgen -- generate --library <liblivesync_ffi.so> --language kotlin ...`
//! (scripts/build-apk.sh runs it). Kept in the workspace so its uniffi version
//! is the one `livesync-ffi` was built with.

fn main() {
    uniffi::uniffi_bindgen_main()
}
