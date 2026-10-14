# crumb's image: built on Debian, run on distroless as a non-root user.
# glibc rather than musl: musl's allocator is slow under a multi-threaded
# server. No CA bundle is needed; webpki-roots compiles the roots in.
FROM docker.io/library/rust:1.96.1-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /src/target/release/crumb /usr/local/bin/crumb
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/crumb"]
