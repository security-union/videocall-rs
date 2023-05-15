FROM rust:1.53
WORKDIR /usr/src/bot
COPY . .
RUN cargo build --release
ENV N_CLIENTS=1 ENDPOINT=ws://example.com/lobby ROOM=default
CMD ["cargo", "run", "--release"]