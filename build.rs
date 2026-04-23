// ========== DOSYA: sentinel-api/build.rs ==========
fn main() -> std::io::Result<()> {
    prost_build::compile_protos(
        &[
            "sentinel-spec/proto/sentinel/market/v1/market_data.proto",
            "sentinel-spec/proto/sentinel/execution/v1/execution.proto",
            "sentinel-spec/proto/sentinel/wallet/v1/wallet.proto", // YENİ EKLENDİ
            "sentinel-spec/proto/sentinel/api/v1/bundle.proto",
        ],
        &["sentinel-spec/proto/"],
    )?;
    Ok(())
}
