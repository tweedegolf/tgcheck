FROM ghcr.io/tweedegolf/debian:bookworm

ARG TARGETARCH

RUN useradd -c "Application" -m -U app
ENV ROOT_SWITCH_USER=app
ENV VERSION=$VERSION
ENV TZ=Europe/Amsterdam
WORKDIR /app
USER app

# copy executable
COPY tgcheck.$TARGETARCH /usr/local/bin/tgcheck
RUN chmod 0755 /usr/local/bin/tgcheck

# run tgcheck
CMD ["/usr/local/bin/tgcheck"]
