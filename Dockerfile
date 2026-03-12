FROM rust:1.85-alpine3.20 AS build
WORKDIR /app

RUN apk add --no-cache build-base

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release

FROM alpine:3.20
RUN apk add --no-cache ca-certificates \
    && addgroup -S app \
    && adduser -S -G app -u 10001 app \
    && mkdir -p /data \
    && chown -R app:app /data

COPY --from=build /app/target/release/gsheet /usr/local/bin/gsheet

VOLUME ["/data"]
EXPOSE 8080
USER app

CMD ["gsheet"]
