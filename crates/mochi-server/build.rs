// The web UI is embedded from apps/web/dist; rebuild when `pnpm build` changes it.
fn main() {
    println!("cargo:rerun-if-changed=../../apps/web/dist");
}
