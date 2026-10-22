FROM rust:1.90-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY skills ./skills
RUN cargo build --release --locked

FROM scratch
COPY --from=build /src/target/release/beacon /usr/local/bin/beacon
ENV BEACON_DATA=/data
EXPOSE 7700
HEALTHCHECK --interval=30s --timeout=5s CMD ["beacon","health"]
CMD ["beacon","serve"]
