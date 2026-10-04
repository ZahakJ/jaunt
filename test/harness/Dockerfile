# jaunt-harness:latest -- one image for relay, routers and machines.
# Based on archlinux so that binaries built on the (Arch, glibc 2.44) dev host run unchanged.
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed \
        iproute2 iptables-nft nftables conntrack-tools tmux openssh socat iputils curl procps-ng python \
        bind less \
 && pacman -Scc --noconfirm \
 && rm -rf /var/cache/pacman/pkg/* /usr/share/man/* /usr/share/doc/* \
 && ssh-keygen -A \
 && useradd -m -s /bin/bash jaunt \
 && mkdir -p /opt/jaunt/bin
ENV PATH=/opt/jaunt/bin:$PATH
CMD ["sleep", "infinity"]
