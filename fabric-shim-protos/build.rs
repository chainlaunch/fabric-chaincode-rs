/// The generated bindings under `src/generated/` are committed so that
/// building this crate (and anything depending on it) never requires
/// `protoc`. This build script is a no-op unless the `regenerate-protos`
/// feature is enabled, in which case it recompiles the vendored `.proto`
/// files to `OUT_DIR` (see `src/lib.rs`, which switches its `include!`
/// source based on the same feature) — used only to refresh
/// `src/generated/` after touching `fabric-shim-protos/protos/`.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "regenerate-protos")]
    {
        tonic_prost_build::configure()
            .build_server(true)
            .build_client(true)
            // The `Chaincode` service has an rpc named `Connect`, which
            // collides with the transport-feature `connect(dst)` client
            // constructor.
            .build_transport(false)
            .compile_protos(
                &[
                    "protos/peer/chaincode_shim.proto",
                    "protos/peer/proposal_response.proto",
                    "protos/common/common.proto",
                    "protos/msp/identities.proto",
                    "protos/ledger/queryresult/kv_query_result.proto",
                ],
                &["protos"],
            )?;
    }
    Ok(())
}
