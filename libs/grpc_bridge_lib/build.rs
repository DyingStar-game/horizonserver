use tonic_prost_build::configure;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    configure()
         .build_server(false)
         .compile_protos(
             &["proto/bridge.proto"],
             &["proto"],
         )?;
    Ok(())
 }