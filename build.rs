fn main() {
    println!("cargo:rerun-if-env-changed=HYPERLIQUID_DEFAULT_BUILDER_ADDRESS");
    println!("cargo:rerun-if-env-changed=HYPERLIQUID_DEFAULT_BUILDER_FEE_RATE");
    println!("cargo:rerun-if-env-changed=HYPERLIQUID_DEFAULT_REFERRAL_CODE");
}
