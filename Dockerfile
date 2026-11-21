FROM nixos/nix:2.34.7@sha256:898e3874bc80a8fbd7df6001b6c83d6e0c904a942e3a4cdf8a89881458333cac AS nix
FROM ubuntu:24.04

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bzip2 ca-certificates curl fonts-freefont-ttf fonts-ipafont-gothic fonts-liberation fonts-noto-color-emoji fonts-tlwg-loma-otf fonts-unifont fonts-wqy-zenhei git gnupg jq libasound2t64 libatk-bridge2.0-0t64 libatk1.0-0t64 libatomic1 libatspi2.0-0t64 libcairo2 libcups2t64 libdbus-1-3 libdrm2 libfontconfig1 libfreetype6 libgbm1 libglib2.0-0t64 libicu74 libkrb5-3 libnspr4 libnss3 libpango-1.0-0 libsecret-1-dev libssl3t64 libx11-6 libxcb1 libxcomposite1 libxdamage1 libxext6 libxfixes3 libxkbcommon0 libxrandr2 nftables pkg-config unzip util-linux xfonts-cyrillic xfonts-scalable xvfb xz-utils zlib1g zstd \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1001 gha \
    && useradd --uid 1001 --gid gha --home-dir /runner/home --shell /bin/bash gha \
    && ln -s /runner/home /home/runner \
    && ln -s /runner/home /home/gha

COPY --from=nix --chown=gha:gha /nix /nix
ARG NODE_VERSION=24.22.3
ARG NODE_SHA256=faf4815c1caac7dcf36330c1c0fcda0ecff7168faba5b4859df2bf9c0958b739
RUN set -eux; \
    mkdir -p /opt/hostedtoolcache/node/$NODE_VERSION/x64; \
    curl -fsSL --retry 3 -o /tmp/node.tar.gz \
      "https://github.com/actions/node-versions/releases/download/${NODE_VERSION}-43134179821/node-${NODE_VERSION}-linux-x64.tar.gz"; \
    echo "$NODE_SHA256  /tmp/node.tar.gz" | sha256sum -c; \
    tar -xzf /tmp/node.tar.gz --strip-components=1 -C /opt/hostedtoolcache/node/$NODE_VERSION/x64; \
    rm /tmp/node.tar.gz; \
    touch /opt/hostedtoolcache/node/$NODE_VERSION/x64.complete
RUN ln -s /opt/hostedtoolcache/node/$NODE_VERSION/x64/bin/node /usr/local/bin/node \
    && ln -s /opt/hostedtoolcache/node/$NODE_VERSION/x64/bin/npm /usr/local/bin/npm \
    && ln -s /opt/hostedtoolcache/node/$NODE_VERSION/x64/bin/npx /usr/local/bin/npx
ARG NODE26_VERSION=26.13.1
ARG NODE26_SHA256=ea9cdfe45ccc6781733f6d7577485b0bd688092db0e0d076dc478c3f59c42362
RUN set -eux; \
    mkdir -p /opt/hostedtoolcache/node/$NODE26_VERSION/x64; \
    curl -fsSL --retry 3 -o /tmp/node26.tar.gz \
      "https://github.com/actions/node-versions/releases/download/${NODE26_VERSION}-43134154182/node-${NODE26_VERSION}-linux-x64.tar.gz"; \
    echo "$NODE26_SHA256  /tmp/node26.tar.gz" | sha256sum -c; \
    tar -xzf /tmp/node26.tar.gz --strip-components=1 -C /opt/hostedtoolcache/node/$NODE26_VERSION/x64; \
    rm /tmp/node26.tar.gz; \
    "/opt/hostedtoolcache/node/$NODE26_VERSION/x64/bin/node" --version; \
    touch /opt/hostedtoolcache/node/$NODE26_VERSION/x64.complete
RUN set -eu; \
    nix_bin="$(find /nix/store -path '*/bin/nix' -type f -print -quit)"; \
    test -n "$nix_bin"; \
    ln -s "${nix_bin%/nix}"/nix* /usr/local/bin/; \
    install -d /etc/nix; \
    printf '%s\n' 'experimental-features = nix-command flakes' 'build-users-group =' \
        'sandbox = false' 'max-jobs = 2' 'cores = 2' > /etc/nix/nix.conf; \
    nix-env --profile /opt/cachix-profile --install --quiet --attr cachix \
        --file https://cachix.org/api/v1/install; \
    ln -s /opt/cachix-profile/bin/cachix /usr/local/bin/cachix; \
    runuser -u gha -- nix --version; \
    runuser -u gha -- cachix --version

VOLUME ["/nix"]

RUN set -eux; \
    archive=actions-runner-linux-x64-2.338.2.tar.gz; \
    curl -fsSL --retry 3 -o "/tmp/$archive" "https://github.com/actions/runner/releases/download/v2.338.2/$archive"; \
    echo "baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40  /tmp/$archive" | sha256sum -c; \
    install -d /opt/actions-runner; \
    tar -xzf "/tmp/$archive" -C /opt/actions-runner; \
    rm "/tmp/$archive"

COPY entrypoint.sh /usr/local/bin/runner-entrypoint
COPY runner.nft /etc/runner.nft
RUN chmod 0755 /usr/local/bin/runner-entrypoint
ENTRYPOINT ["/usr/local/bin/runner-entrypoint"]
