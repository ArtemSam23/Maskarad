# syntax=docker/dockerfile:1
FROM rust:1.94-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY data ./data
RUN cargo build --release --locked && strip target/release/maskarad

FROM gcr.io/distroless/cc-debian12:nonroot
WORKDIR /app
COPY --from=builder /src/target/release/maskarad /app/maskarad
COPY config/maskarad.yaml /app/config/maskarad.yaml
ENV MASKARAD_LISTEN=0.0.0.0:8080
EXPOSE 8080
USER 65532:65532
ENTRYPOINT ["/app/maskarad"]
CMD ["serve", "--config", "/app/config/maskarad.yaml"]
