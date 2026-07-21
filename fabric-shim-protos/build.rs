fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        // The `Chaincode` service has an rpc named `Connect`, which collides
        // with the transport-feature `connect(dst)` client constructor.
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
    Ok(())
}
