FROM ubuntu:22.04

ARG USER
ARG UID

# Install flatbuffers compiler and dependencies
RUN apt-get update && apt-get install -y \
    flatbuffers-compiler \
    curl \
    build-essential \
    && rm -rf /var/lib/apt/lists/*

# Create user with same UID as host user
RUN useradd -m -u ${UID} -s /bin/bash ${USER}

USER ${USER}
WORKDIR /workspace

CMD ["/bin/bash"]
