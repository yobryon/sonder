# sonder-relay: a ~10 MB image. Build from the repository root:
#   docker build -f crates/sonder-relay/Dockerfile -t sonder-relay .
#   docker run -p 8788:8788 sonder-relay
FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p sonder-relay && strip target/release/sonder-relay

FROM gcr.io/distroless/cc-debian12
COPY --from=build /src/target/release/sonder-relay /sonder-relay
EXPOSE 8788
ENTRYPOINT ["/sonder-relay", "--bind", "0.0.0.0:8788"]
