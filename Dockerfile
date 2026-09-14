# Runtime image: the release binary is copied in by CI (`dist/vauban`).
# linux/amd64 and linux/arm64 are built natively, then joined into one manifest.
FROM debian:bookworm-slim

ARG VAUBAN_VERSION=0.0.0
ARG VAUBAN_REVISION=unknown

LABEL org.opencontainers.image.title="VaubanDB" \
      org.opencontainers.image.description="VaubanDB: an independent database server that speaks TDS and T-SQL" \
      org.opencontainers.image.source="https://github.com/ExLighthalzen/VaubanDB" \
      org.opencontainers.image.licenses="BSD-3-Clause" \
      org.opencontainers.image.version="${VAUBAN_VERSION}" \
      org.opencontainers.image.revision="${VAUBAN_REVISION}"

RUN groupadd --gid 1000 vauban \
    && useradd --uid 1000 --gid vauban --home-dir /var/lib/vauban \
        --create-home --shell /usr/sbin/nologin vauban \
    && mkdir -p /usr/share/doc/vauban \
    && chown vauban:vauban /var/lib/vauban

COPY dist/vauban /usr/local/bin/vauban
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
COPY LICENSE /usr/share/doc/vauban/LICENSE

RUN chmod 755 /usr/local/bin/vauban /usr/local/bin/entrypoint.sh

USER vauban
WORKDIR /var/lib/vauban

EXPOSE 1433
VOLUME /var/lib/vauban

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
