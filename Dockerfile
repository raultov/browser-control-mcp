# --- Build stage ---
FROM rust:1.98-slim-bookworm AS builder
WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release --locked

# --- Runtime: Google Chrome stable and Firefox (Mozilla's APT repository) ---
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        curl gnupg ca-certificates xz-utils procps \
    && curl -fsSL https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb -o /tmp/chrome.deb \
    && apt-get install -y --no-install-recommends /tmp/chrome.deb \
    && rm /tmp/chrome.deb \
    && install -d -m 0755 /etc/apt/keyrings \
    && curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg -o /etc/apt/keyrings/packages.mozilla.org.asc \
    && echo "deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main" \
        > /etc/apt/sources.list.d/mozilla.list \
    && printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' > /etc/apt/preferences.d/mozilla \
    && apt-get update && apt-get install -y --no-install-recommends firefox \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/browser-control-mcp /usr/local/bin/browser-control-mcp

# MCP speaks over stdio; headless is required in a container. Pick Firefox
# per instance (open_instance browser: "firefox") or with --browser firefox.
ENTRYPOINT ["browser-control-mcp"]
CMD ["--headless"]
