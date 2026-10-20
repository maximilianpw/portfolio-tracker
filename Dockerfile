FROM rust:1.97-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY .sqlx ./.sqlx
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked

FROM python:3.13-slim-bookworm
RUN pip install --no-cache-dir pytr==0.4.10 \
    && useradd --uid 10001 --home-dir /data --create-home --shell /usr/sbin/nologin tracker
COPY --from=build /src/target/release/portfolio-tracker /usr/local/bin/portfolio-tracker
USER tracker
WORKDIR /data
ENV HOME=/data \
    PT_DATA_DIR=/data \
    PT_LISTEN=0.0.0.0:8787
VOLUME /data
EXPOSE 8787
CMD ["portfolio-tracker", "serve", "--token-file", "/data/token"]
