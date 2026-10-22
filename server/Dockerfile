# Build: docker build -t floki-server .   Run: docker run -e DATABASE_URL=... -p 8787:8787 floki-server
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY migrations migrations
RUN cargo build --release --locked

# TLS roots are compiled in (webpki), so no CA bundle or OpenSSL is needed at run time.
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/floki-server /usr/local/bin/floki-server
ENV PORT=8787
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/floki-server"]
