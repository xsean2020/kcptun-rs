// Build script: enforce tokio / smol feature mutual exclusion at build time.
//
// This is the canonical enforcement point for the dual-runtime architecture.
// The `compile_error!` in src/lib.rs is a secondary safety net.
fn main() {
    let tokio_enabled = std::env::var("CARGO_FEATURE_TOKIO").is_ok();
    let smol_enabled = std::env::var("CARGO_FEATURE_SMOL").is_ok();

    if tokio_enabled && smol_enabled {
        panic!(
            "\n[CRITICAL ERROR] Feature conflict: `tokio` and `smol` are mutually exclusive!\n\
             Use `--no-default-features --features smol` to select a single runtime.\n"
        );
    }

    if !tokio_enabled && !smol_enabled {
        panic!(
            "\n[CRITICAL ERROR] Missing dependency: enable either `tokio` or `smol` feature!\n"
        );
    }
}
