fn main() -> std::io::Result<()> {
    // UI'ın ihtiyaç duyduğu tüm proto'ları derle
    prost_build::compile_protos(
        &[
            "sentinel-spec/proto/sentinel/market/v1/market_data.proto",
            "sentinel-spec/proto/sentinel/execution/v1/execution.proto",
        ],
        &["sentinel-spec/proto/"],
    )?;
    Ok(())
}
