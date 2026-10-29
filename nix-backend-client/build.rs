fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The backend owns the contract; every frontend compiles the same file.
    let proto_dir = "../nix-backend-generic/proto";
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile(&[format!("{proto_dir}/nix_backend.proto")], &[proto_dir])?;
    Ok(())
}
