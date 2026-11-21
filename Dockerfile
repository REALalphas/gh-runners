FROM ubuntu:24.04

RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        bzip2 ca-certificates curl git gnupg libicu74 libkrb5-3 libssl3t64 nftables unzip util-linux xz-utils zlib1g \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1001 gha \
    && useradd --uid 1001 --gid gha --home-dir /runner/home --shell /bin/bash gha

RUN mkdir -m 0755 /nix \
    && curl -fsSL --retry 3 https://nixos.org/nix/install | sh -s -- --no-daemon \
    && printf 'experimental-features = nix-command flakes\n' > /etc/nix/nix.conf

ENV PATH="/nix/var/nix/profiles/default/bin:/nix/var/nix/profiles/per-user/root/profile/bin:$PATH"

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
