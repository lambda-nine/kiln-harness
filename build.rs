fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/kilnd/v1/kilnd.proto";
    println!("cargo:rerun-if-changed={proto}");
    // Ordered maps encode in key order, so a retried create is the same
    // bytes on the wire as the first attempt, not only an equal request.
    tonic_prost_build::configure()
        .btree_map(".")
        .compile_protos(&[proto], &["proto"])?;
    Ok(())
}
