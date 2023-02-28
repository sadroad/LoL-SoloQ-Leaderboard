FROM rust:1.67 as builder
WORKDIR /usr/src/soloqbot
COPY Cargo.toml Cargo.lock ./
RUN echo "fn main() {}" > dummy.rs
RUN sed -i 's#src/main.rs#dummy.rs#' Cargo.toml
RUN cargo build --release
RUN sed -i 's#dummy.rs#src/main.rs#' Cargo.toml
COPY ./src ./src
RUN touch -a -m ./src/main.rs
RUN cargo install --path .

FROM debian:bullseye-slim
RUN apt-get update && apt-get install -y ca-certificates
COPY --from=builder /usr/local/cargo/bin/soloqbot /usr/local/bin/soloqbot
COPY .env .env
CMD ["soloqbot"]