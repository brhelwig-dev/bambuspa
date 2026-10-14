# Built natively on the image's own architecture. Alpine's toolchain is musl,
# so the binary is static and runs on distroless/static.
FROM rust:1.98-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY . .
RUN cargo build --release --locked && cp target/release/bambuspa /bambuspa

FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /bambuspa /bambuspa
EXPOSE 8081
ENTRYPOINT ["/bambuspa"]
