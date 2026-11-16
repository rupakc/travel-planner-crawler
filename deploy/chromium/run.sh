#!/bin/sh
# Entrypoint for the chromedp/headless-shell container used by vc-render (spec §5.4, §17.1).
#
# Chromium binds its DevTools endpoint to 127.0.0.1 only (current headless-shell builds ignore
# --remote-debugging-address), so socat forwards the container's :9222 to it. Chrome's own
# sandbox stays ON (no --no-sandbox): run with deploy/chromium/seccomp.json, non-root,
# read-only root filesystem, /tmp as tmpfs. The container sits on the internal `render`
# network: it has no route out, and the fetcher answers every request it makes through CDP
# Fetch interception, under the page's permit.
#
# The vc-render integration tests start the pinned image with exactly this script.
set -eu

socat TCP4-LISTEN:9222,fork,reuseaddr TCP4:127.0.0.1:9223 &

exec /headless-shell/headless-shell \
  --headless \
  --remote-debugging-port=9223 \
  --disable-gpu \
  --disable-dev-shm-usage \
  --disable-background-networking \
  --disable-component-update \
  --disable-sync \
  --disable-default-apps \
  --disable-extensions \
  --no-first-run \
  --no-default-browser-check \
  --metrics-recording-only \
  --user-data-dir=/tmp/profile \
  about:blank
