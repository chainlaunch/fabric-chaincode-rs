# Build from the workspace root:
#   docker build -f examples/asset-transfer/Dockerfile -t chainlaunch/rust-cc-asset-transfer:0.1 .
FROM rust:1-slim-bookworm AS build
# No protoc needed: fabric-shim-protos builds from the bindings committed
# under src/generated/ by default (see its build.rs / regenerate-protos
# feature for the opt-in codegen path).
WORKDIR /src
COPY . .
RUN cargo build --release -p asset-transfer

# distroless/cc: glibc + no shell, runs as nonroot
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/asset-transfer /usr/local/bin/asset-transfer
# ChainLaunch injects CHAINCODE_ID and CHAINCODE_SERVER_ADDRESS at deploy time.
ENV CHAINCODE_SERVER_ADDRESS=0.0.0.0:7052
EXPOSE 7052
ENTRYPOINT ["/usr/local/bin/asset-transfer"]
