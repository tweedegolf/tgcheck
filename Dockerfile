FROM docker.tgrep.nl/docker/debian:bookworm
RUN useradd -c "Application" -m -U app
ENV ROOT_SWITCH_USER app
ENV VERSION $VERSION
ENV TZ Europe/Amsterdam
WORKDIR /app
USER app
COPY ./target/release/tgcheck /usr/bin/tgcheck
ENTRYPOINT [ "docker-entrypoint", "/usr/bin/tgcheck" ]
